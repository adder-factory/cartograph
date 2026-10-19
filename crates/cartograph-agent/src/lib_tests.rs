use super::*;

#[path = "native_read_tests.rs"]
mod native_read_tests;

#[test]
fn worker_selector_is_corpus_aware_bounded_and_monotonic() {
    const OVERSIZED_CORPUS: usize = EIGHT_WORKER_MAX_FILES * EIGHT_WORKER_MAX_FILES;
    const CALLER_CAP: u16 = 3;

    let hardware = std::thread::available_parallelism()
        .ok()
        .and_then(|value| u16::try_from(value.get()).ok())
        .unwrap_or(ONE_WORKER)
        .max(ONE_WORKER);
    let cases = [
        (0, ONE_WORKER),
        (SINGLE_WORKER_MAX_FILES, ONE_WORKER),
        (SINGLE_WORKER_MAX_FILES + 1, TWO_WORKERS),
        (TWO_WORKER_MAX_FILES + 1, FOUR_WORKERS),
        (FOUR_WORKER_MAX_FILES + 1, EIGHT_WORKERS),
        (EIGHT_WORKER_MAX_FILES + 1, MAX_CONFIGURED_WORKERS),
    ];
    let mut prior = 0;
    for (files, requested) in cases {
        let selected = select_worker_count(files, 0, MAX_CONFIGURED_WORKERS);
        assert_eq!(selected, requested.min(hardware));
        assert!(selected >= prior);
        prior = selected;
    }
    let byte_cases = [
        (0, ONE_WORKER),
        (SINGLE_WORKER_MAX_SOURCE_BYTES, ONE_WORKER),
        (SINGLE_WORKER_MAX_SOURCE_BYTES + 1, TWO_WORKERS),
        (TWO_WORKER_MAX_SOURCE_BYTES + 1, FOUR_WORKERS),
        (FOUR_WORKER_MAX_SOURCE_BYTES + 1, EIGHT_WORKERS),
        (EIGHT_WORKER_MAX_SOURCE_BYTES + 1, MAX_CONFIGURED_WORKERS),
    ];
    let mut prior = 0;
    for (source_bytes, requested) in byte_cases {
        let selected = select_worker_count(1, source_bytes, MAX_CONFIGURED_WORKERS);
        assert_eq!(selected, requested.min(hardware));
        assert!(selected >= prior);
        prior = selected;
    }
    assert_eq!(
        select_worker_count(34, 1_052_564, MAX_CONFIGURED_WORKERS),
        EIGHT_WORKERS.min(hardware)
    );
    assert_eq!(
        select_worker_count(256, 6_145_536, MAX_CONFIGURED_WORKERS),
        MAX_CONFIGURED_WORKERS.min(hardware)
    );
    assert_eq!(
        select_worker_count(OVERSIZED_CORPUS, u64::MAX, CALLER_CAP),
        CALLER_CAP.min(hardware)
    );
}

#[test]
fn project_identity_is_stable_distinct_and_does_not_expose_the_path() {
    let first = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let second = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let first_path = std::fs::canonicalize(first.path())
        .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));
    let second_path = std::fs::canonicalize(second.path())
        .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));
    let identity = project_identity_digest(&first_path);
    assert_eq!(identity, project_identity_digest(&first_path));
    assert_ne!(identity, project_identity_digest(&second_path));
    assert!(
        !identity
            .as_str()
            .contains(&first_path.to_string_lossy().to_string())
    );
}

#[test]
fn source_revision_changes_with_supported_content_and_ignores_local_state() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let source = directory.path().join("service.ts");
    std::fs::write(&source, "export const value = 1;\n")
        .unwrap_or_else(|error| panic!("source write failed: {error}"));
    let first = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("source revision failed: {error}"));
    std::fs::write(directory.path().join(".editorconfig"), "root = true\n")
        .unwrap_or_else(|error| panic!("unrelated dotfile write failed: {error}"));
    let unrelated_dotfile = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("dotfile source revision failed: {error}"));
    assert_eq!(first.digest, unrelated_dotfile.digest);
    std::fs::create_dir(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("state dir failed: {error}"));
    std::fs::write(directory.path().join(".cartograph/cache"), "private state")
        .unwrap_or_else(|error| panic!("state write failed: {error}"));
    let ignored = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("source revision failed: {error}"));
    assert_eq!(first.digest, ignored.digest);
    std::fs::write(&source, "export const value = 2;\n")
        .unwrap_or_else(|error| panic!("source write failed: {error}"));
    let changed = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("source revision failed: {error}"));
    assert_ne!(first.digest, changed.digest);
    assert_eq!(changed.files, 1);
    std::fs::create_dir_all(directory.path().join(".cartograph/scip"))
        .unwrap_or_else(|error| panic!("SCIP state dir failed: {error}"));
    let overlay_bytes = [0x0a_u8, 0x00];
    std::fs::write(
        directory.path().join(SCIP_OVERLAY_RELATIVE_PATH),
        overlay_bytes,
    )
    .unwrap_or_else(|error| panic!("SCIP overlay write failed: {error}"));
    let overlaid = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("overlaid source revision failed: {error}"));
    assert_ne!(changed.digest, overlaid.digest);
    assert_eq!(changed.files, overlaid.files);
    let retained = source_revision_with_options(
        SourceRevisionRequest {
            root: directory.path(),
            capture_paths: &BTreeSet::new(),
            retain_scip_overlay: true,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            discovery_policy: DiscoveryPolicy::v1_defaults()
                .unwrap_or_else(|error| panic!("default discovery policy failed: {error}")),
            index_policy: SourceIndexPolicy::full(duplicate_code_allowlist_digest(&[])),
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("retained overlay scan failed: {error}"));
    assert_eq!(retained.digest, overlaid.digest);
    assert_eq!(
        retained
            .scip_overlay
            .as_ref()
            .map(|overlay| overlay.content_digest().clone()),
        Some(ContentDigest::from_bytes(
            *blake3::hash(&overlay_bytes).as_bytes()
        ))
    );
}

#[test]
fn source_revision_honors_live_project_excludes_and_ignores_excluded_edits() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::create_dir_all(directory.path().join("private"))
        .unwrap_or_else(|error| panic!("private directory failed: {error}"));
    std::fs::create_dir_all(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("config directory failed: {error}"));
    std::fs::write(
        directory.path().join("public.ts"),
        "export const visible = true;\n",
    )
    .unwrap_or_else(|error| panic!("public source failed: {error}"));
    std::fs::write(
        directory.path().join("private/hidden.ts"),
        "export const hidden = 1;\n",
    )
    .unwrap_or_else(|error| panic!("private source failed: {error}"));
    std::fs::write(
        directory.path().join(".cartograph/config.json"),
        r#"{"exclude":["private/**"]}"#,
    )
    .unwrap_or_else(|error| panic!("project config failed: {error}"));
    let excluded = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("excluded revision failed: {error}"));
    assert_eq!(excluded.files, 1);
    std::fs::write(
        directory.path().join("private/hidden.ts"),
        "export const hidden = 2;\n",
    )
    .unwrap_or_else(|error| panic!("private edit failed: {error}"));
    let excluded_edit = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("excluded edit revision failed: {error}"));
    assert_eq!(excluded.digest, excluded_edit.digest);
    std::fs::write(directory.path().join(".cartograph/config.json"), "{}")
        .unwrap_or_else(|error| panic!("project config reset failed: {error}"));
    let included = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("included revision failed: {error}"));
    assert_eq!(included.files, 2);
    assert_ne!(included.digest, excluded.digest);
}

#[test]
fn source_revision_fences_graph_analysis_policy_changes() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
        .unwrap_or_else(|error| panic!("source write failed: {error}"));
    let baseline = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("baseline revision failed: {error}"));
    std::fs::create_dir(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("config directory failed: {error}"));
    let config = directory.path().join(".cartograph/config.json");
    std::fs::write(&config, r#"{"enableCentrality":false}"#)
        .unwrap_or_else(|error| panic!("centrality config failed: {error}"));
    let without_page_rank = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("centrality revision failed: {error}"));
    assert_ne!(baseline.digest, without_page_rank.digest);

    std::fs::write(
        &config,
        r#"{"enableCentrality":false,"enableBetweenness":false}"#,
    )
    .unwrap_or_else(|error| panic!("betweenness config failed: {error}"));
    let without_graph_scores = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("betweenness revision failed: {error}"));
    assert_ne!(without_page_rank.digest, without_graph_scores.digest);

    std::fs::write(
        &config,
        r#"{"enableCentrality":false,"enableBetweenness":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("history config failed: {error}"));
    let auxiliary_change = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("auxiliary revision failed: {error}"));
    assert_eq!(without_graph_scores.digest, auxiliary_change.digest);

    std::fs::write(
        &config,
        r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false}"#,
    )
    .unwrap_or_else(|error| panic!("docstring config failed: {error}"));
    let without_docstrings = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("docstring revision failed: {error}"));
    assert_ne!(without_graph_scores.digest, without_docstrings.digest);

    std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false}"#,
        )
        .unwrap_or_else(|error| panic!("call-site config failed: {error}"));
    let without_call_sites = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("call-site revision failed: {error}"));
    assert_ne!(without_docstrings.digest, without_call_sites.digest);

    std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true}"#,
        )
        .unwrap_or_else(|error| panic!("partial-clone config failed: {error}"));
    let wider_clone_band = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("partial-clone revision failed: {error}"));
    assert_ne!(without_call_sites.digest, wider_clone_band.digest);

    std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true,"duplicateCodeAllowlist":["generated/**"]}"#,
        )
        .unwrap_or_else(|error| panic!("clone allowlist config failed: {error}"));
    let clone_allowlist = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("clone allowlist revision failed: {error}"));
    assert_ne!(wider_clone_band.digest, clone_allowlist.digest);

    std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true,"duplicateCodeAllowlist":["generated/**"],"maxAstDepth":512}"#,
        )
        .unwrap_or_else(|error| panic!("AST depth config failed: {error}"));
    let deeper_ast = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("AST depth revision failed: {error}"));
    assert_ne!(clone_allowlist.digest, deeper_ast.digest);
}

#[test]
fn source_revision_honors_the_configured_language_allow_list() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
        .unwrap_or_else(|error| panic!("Rust fixture failed: {error}"));
    let typescript = directory.path().join("ignored.ts");
    std::fs::write(&typescript, "export const ignored = 1;\n")
        .unwrap_or_else(|error| panic!("TypeScript fixture failed: {error}"));
    std::fs::create_dir(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("config directory failed: {error}"));
    let config = directory.path().join(".cartograph/config.json");
    std::fs::write(&config, r#"{"languages":["rust"]}"#)
        .unwrap_or_else(|error| panic!("language config failed: {error}"));
    let rust_only = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("Rust-only revision failed: {error}"));
    assert_eq!(rust_only.files, 1);
    std::fs::write(&typescript, "export const ignored = 2;\n")
        .unwrap_or_else(|error| panic!("ignored edit failed: {error}"));
    let ignored_edit = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("ignored revision failed: {error}"));
    assert_eq!(rust_only.digest, ignored_edit.digest);

    std::fs::write(&config, r#"{"languages":[]}"#)
        .unwrap_or_else(|error| panic!("all-language config failed: {error}"));
    let all_languages = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("all-language revision failed: {error}"));
    assert_eq!(all_languages.files, 2);
    assert_ne!(rust_only.digest, all_languages.digest);
}

#[test]
fn source_revision_honors_include_globs_and_migrates_v1_additions() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::create_dir_all(directory.path().join("src"))
        .unwrap_or_else(|error| panic!("source directory failed: {error}"));
    std::fs::create_dir_all(directory.path().join("tests"))
        .unwrap_or_else(|error| panic!("test directory failed: {error}"));
    std::fs::create_dir(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("config directory failed: {error}"));
    std::fs::write(directory.path().join("src/lib.rs"), "pub fn ready() {}\n")
        .unwrap_or_else(|error| panic!("source fixture failed: {error}"));
    let excluded = directory.path().join("tests/lib.rs");
    std::fs::write(&excluded, "#[test] fn ready() {}\n")
        .unwrap_or_else(|error| panic!("test fixture failed: {error}"));
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname='fixture'\n",
    )
    .unwrap_or_else(|error| panic!("TOML fixture failed: {error}"));
    let config = directory.path().join(".cartograph/config.json");
    std::fs::write(&config, r#"{"version":2,"include":["src/**/*.rs"]}"#)
        .unwrap_or_else(|error| panic!("include config failed: {error}"));
    let included = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("included revision failed: {error}"));
    assert_eq!(included.files, 1);
    std::fs::write(&excluded, "#[test] fn changed() {}\n")
        .unwrap_or_else(|error| panic!("excluded edit failed: {error}"));
    let excluded_edit = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("excluded edit revision failed: {error}"));
    assert_eq!(included.digest, excluded_edit.digest);

    std::fs::write(&config, r#"{"version":1,"include":["src/**/*.rs"]}"#)
        .unwrap_or_else(|error| panic!("v1 include config failed: {error}"));
    let migrated = source_revision(directory.path())
        .unwrap_or_else(|error| panic!("migrated include revision failed: {error}"));
    assert_eq!(migrated.files, 2);
    assert_ne!(included.digest, migrated.digest);
}

#[cfg(unix)]
#[test]
fn source_revision_rejects_a_symlinked_scip_overlay() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
        .unwrap_or_else(|error| panic!("source write failed: {error}"));
    std::fs::create_dir_all(directory.path().join(".cartograph/scip"))
        .unwrap_or_else(|error| panic!("SCIP state dir failed: {error}"));
    let external = directory.path().join("foreign.scip");
    std::fs::write(&external, [0x0a_u8, 0x00])
        .unwrap_or_else(|error| panic!("external write failed: {error}"));
    symlink(&external, directory.path().join(SCIP_OVERLAY_RELATIVE_PATH))
        .unwrap_or_else(|error| panic!("symlink failed: {error}"));
    assert_eq!(
        source_revision(directory.path()).map(|source| source.digest),
        Err(ProjectError::ScipOverlayInvalid)
    );
}

#[test]
fn public_source_identity_matches_internal_revision_without_leaking_root_text() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::write(
        directory.path().join("service.ts"),
        "export const ready = true;\n",
    )
    .unwrap_or_else(|error| panic!("source write failed: {error}"));
    std::fs::write(
        directory.path().join("service.pyi"),
        "def ready() -> bool: ...\n",
    )
    .unwrap_or_else(|error| panic!("additive Python source write failed: {error}"));
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap_or_else(|error| panic!("additive TOML source write failed: {error}"));
    let canonical = std::fs::canonicalize(directory.path())
        .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));

    let identity = ProjectRuntime::inspect_source_identity(directory.path())
        .unwrap_or_else(|error| panic!("source identity failed: {error}"));
    let internal = source_revision(&canonical)
        .unwrap_or_else(|error| panic!("source revision failed: {error}"));

    assert_eq!(identity.source_revision, internal.digest);
    assert_eq!(identity.v1_source_manifest, internal.v1_source_manifest);
    let service_path = NormalizedPath::parse("service.ts")
        .unwrap_or_else(|error| panic!("service path was invalid: {error}"));
    let service_hash =
        ContentDigest::from_bytes(*blake3::hash(b"export const ready = true;\n").as_bytes());
    assert_eq!(
        identity.v1_source_manifest,
        finish_source_manifest(&[(service_path, service_hash)])
            .unwrap_or_else(|error| panic!("v1 manifest failed: {error}"))
    );
    assert_eq!(identity.files, 3);
    assert_eq!(
        identity.repository_fingerprint,
        project_identity_digest(&canonical)
    );
    assert!(!format!("{identity:?}").contains(&canonical.to_string_lossy().to_string()));
}

#[test]
fn invalid_worker_caps_fail_before_any_database_or_source_work() {
    assert_eq!(
        IndexOptions::default().with_max_workers(0),
        Err(ProjectError::InvalidOptions)
    );
    assert_eq!(
        IndexOptions::default().with_max_workers(17),
        Err(ProjectError::InvalidOptions)
    );
    assert_eq!(
        IndexOptions::default().with_max_source_bytes(0),
        Err(ProjectError::InvalidOptions)
    );
    assert_eq!(
        IndexOptions::default().with_max_source_bytes(DEFAULT_MAX_SOURCE_BYTES + 1),
        Err(ProjectError::InvalidOptions)
    );
    assert!(
        IndexOptions::default()
            .with_max_source_bytes(10 * 1024 * 1024)
            .is_ok()
    );
}

#[test]
fn source_identity_preserves_named_project_configuration_range_errors() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::create_dir(directory.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("config directory failed: {error}"));
    std::fs::write(
        directory.path().join(".cartograph/config.json"),
        r#"{"maxGenerationBytes":8589934593}"#,
    )
    .unwrap_or_else(|error| panic!("config fixture failed: {error}"));
    let error = ProjectRuntime::inspect_source_identity(directory.path())
        .err()
        .unwrap_or_else(|| panic!("out-of-range source identity unexpectedly succeeded"));
    assert_eq!(
        error,
        ProjectError::ProjectConfiguration(ProjectConfigError::NumericFieldOutOfRange {
            field: "maxGenerationBytes",
            minimum: 1,
            maximum: 8_589_934_592,
        })
    );
    assert!(error.to_string().contains("`maxGenerationBytes`"));
}

#[test]
fn automatic_indexing_is_history_free_and_worker_bounded() {
    let automatic = IndexOptions::automatic();

    assert_eq!(automatic.max_workers, FOUR_WORKERS);
    assert!(!automatic.refresh_history);
    assert_eq!(
        automatic.failure_retention,
        IndexFailureRetention::AutomaticFailures
    );
    assert!(!automatic.force);
    assert_eq!(automatic.max_source_bytes, None);
    assert!(!automatic.profile);
    assert_eq!(automatic.additional_excludes, [] as [String; 0]);
    assert_eq!(
        automatic.admission_reconciliation,
        IndexAdmissionReconciliation::PreserveCurrent
    );
}

#[test]
fn semantic_maintenance_can_skip_only_the_independent_history_refresh() {
    let defaults = IndexOptions::default();
    assert!(defaults.refresh_history);
    assert_eq!(
        defaults.admission_reconciliation,
        IndexAdmissionReconciliation::Replace
    );
    assert_eq!(
        IndexOptions::reconciliation().admission_reconciliation,
        IndexAdmissionReconciliation::PreserveCurrent
    );
    let semantic = defaults.with_history_refresh(false);
    assert!(!semantic.refresh_history);
    assert!(!semantic.force);
    assert_eq!(semantic.max_source_bytes, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_source_scans_cancel_without_starving_runtime_timers() {
    const REQUESTS: usize = 32;
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
        .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
    let root = directory
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("fixture canonicalize failed: {error}"));
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS));
    let held = permits
        .clone()
        .acquire_owned()
        .await
        .unwrap_or_else(|error| panic!("fixture permit failed: {error}"));
    let mut signals = Vec::new();
    let mut scans = Vec::new();
    for _ in 0..REQUESTS {
        let cancellation = ProjectCancellation::new();
        signals.push(cancellation.clone());
        scans.push(tokio::spawn(scan_source_path(SourceScanRequest {
            root: root.clone(),
            permits: permits.clone(),
            counters: Arc::default(),
            capture_paths: BTreeSet::new(),
            retain_scip_overlay: false,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            discovery_policy: DiscoveryPolicy::v1_defaults()
                .unwrap_or_else(|error| panic!("default discovery policy failed: {error}")),
            index_policy: SourceIndexPolicy::full(duplicate_code_allowlist_digest(&[])),
            cancellation,
        })));
    }
    tokio::time::timeout(
        Duration::from_millis(100),
        tokio::time::sleep(Duration::from_millis(1)),
    )
    .await
    .unwrap_or_else(|_| panic!("source scans starved the runtime timer"));
    for signal in &signals {
        signal.cancel();
    }
    let reaped = tokio::time::timeout(Duration::from_secs(1), async {
        for scan in scans {
            let result = scan
                .await
                .unwrap_or_else(|error| panic!("source scan task failed: {error}"));
            assert_eq!(
                result.map(|source| source.files),
                Err(ProjectError::RequestCancelled)
            );
        }
    });
    reaped
        .await
        .unwrap_or_else(|_| panic!("cancelled source scans were not reaped"));
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_checkpoints_advance_with_every_file_a_scan_hashes() {
    const FILES: u64 = 12;
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let root = directory
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("fixture canonicalize failed: {error}"));
    let scan = |counters: Arc<SourceScanCounters>| {
        scan_source_path(SourceScanRequest {
            root: root.clone(),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS)),
            counters,
            capture_paths: BTreeSet::new(),
            retain_scip_overlay: false,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            discovery_policy: DiscoveryPolicy::v1_defaults()
                .unwrap_or_else(|error| panic!("default discovery policy failed: {error}")),
            index_policy: SourceIndexPolicy::full(duplicate_code_allowlist_digest(&[])),
            cancellation: ProjectCancellation::new(),
        })
    };
    let empty = Arc::<SourceScanCounters>::default();
    scan(empty.clone())
        .await
        .unwrap_or_else(|error| panic!("empty scan failed: {error}"));
    let baseline = empty.checkpoints.load(Ordering::Relaxed);

    for index in 0..FILES {
        std::fs::write(
            directory.path().join(format!("service_{index}.rs")),
            format!("pub fn ready_{index}() {{}}\n"),
        )
        .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
    }
    let counters = Arc::<SourceScanCounters>::default();
    let source = scan(counters.clone())
        .await
        .unwrap_or_else(|error| panic!("fixture scan failed: {error}"));
    assert_eq!(source.files, 12);
    assert_eq!(counters.observations.load(Ordering::Relaxed), 1);
    // Work units grow with the corpus, so a long scan keeps reporting
    // progress instead of looking idle until it finishes.
    assert!(counters.checkpoints.load(Ordering::Relaxed) >= baseline + FILES);
}
