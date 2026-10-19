//! Optional warm state belongs to the existing per-project writer admission.

use cartograph_db::CanonicalGenerationFacts;
use cartograph_indexer::{NativeLocalEditRequest, try_native_local_edit};

use super::{
    Arc, DEFAULT_MAX_GENERATION_BYTES, DEFAULT_MAX_SOURCE_BYTES, GenerationId, GraphBase,
    GraphBuild, GraphPublicationInput, GraphRevision, NativeParseCache, PipelineBounds,
    ProjectError, RocksError, RocksProjectError, RocksProjectRuntime, SourceRevision, SourceRoot,
    new_generation_id, pipeline_config, run_source_worker,
};

pub(super) struct WarmGraph {
    pub(super) generation: GenerationId,
    pub(super) policy: crate::SourceIndexPolicy,
    pub(super) scip_overlay: bool,
    pub(super) facts: Arc<CanonicalGenerationFacts>,
}

impl RocksProjectRuntime {
    pub(super) async fn try_local_graph(
        &self,
        warm: &mut Option<WarmGraph>,
        build: &GraphBuild<'_>,
    ) -> Result<Option<(GraphPublicationInput, u16)>, RocksProjectError> {
        let GraphBuild {
            source,
            policy,
            options,
            base,
            request,
            publication_policy,
            progress: _,
            excludes,
            ceiling,
        } = build;
        request.check()?;
        let Some(cached) = warm.as_ref().filter(|cached| {
            !options.force
                // A custom retained ceiling also governs parse/resolve stages.
                // Only the complete builder can prove those stage budgets.
                && policy.maximum_generation_bytes.is_none()
                && !cached.scip_overlay
                && source.scip_overlay.is_none()
                && cached.policy == policy.index
                && *base == GraphBase::Current(cached.generation.clone())
                && source.admitted_manifest.len() == cached.facts.files().len()
        }) else {
            return Ok(None);
        };
        let paths = tokio::task::block_in_place(|| {
            discover_local_paths(&cached.facts, source, || request.check())
        })?;
        let Some(paths) = paths else {
            return Ok(None);
        };
        let root = self.root.clone();
        let discovery = policy.discovery.clone();
        let source_root = run_source_worker(move || {
            SourceRoot::open_with_policy(&root, discovery)
                .map_err(|_| ProjectError::ProjectRootUnavailable)
        })
        .await?;
        let pipeline = pipeline_config(
            PipelineBounds {
                workers: 1,
                max_source_bytes: policy
                    .maximum_file_bytes
                    .unwrap_or(DEFAULT_MAX_SOURCE_BYTES),
                max_generation_bytes: policy
                    .maximum_generation_bytes
                    .unwrap_or(DEFAULT_MAX_GENERATION_BYTES),
            },
            policy.index,
        )?;
        let replacements = try_native_local_edit(
            NativeLocalEditRequest {
                source_root: &source_root,
                base: &cached.facts,
                paths: &paths,
                cache: NativeParseCache::from_native(self.database.clone()),
                config: pipeline,
            },
            request,
        )
        .await
        .map_err(RocksProjectError::Storage)?;
        let Some(replacements) = replacements else {
            return Ok(None);
        };
        let Some(cached) = warm.take() else {
            return Ok(None);
        };
        let Ok(facts) = Arc::try_unwrap(cached.facts) else {
            return Ok(None);
        };
        let identity = GraphRevision {
            generation_id: new_generation_id(),
            source_revision: source.digest.clone(),
            base: base.clone(),
        };
        let candidate = tokio::task::block_in_place(|| {
            GraphPublicationInput::replace_files(
                identity,
                facts,
                replacements,
                *publication_policy,
                request,
            )
        });
        request.check()?;
        // An exhausted optional fast path does not reduce the complete build's
        // admission policy or turn a valid edit into an indexing failure.
        let Ok(input) = candidate else {
            return Ok(None);
        };
        let admitted = tokio::task::block_in_place(|| {
            input.with_source_admission(excludes.to_vec(), *ceiling)
        });
        request.check()?;
        let files = u16::try_from(paths.len()).map_err(|_| RocksProjectError::InvalidQuery)?;
        match admitted {
            Ok(input) => Ok(Some((input, files))),
            Err(RocksError::Limit { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn discover_local_paths(
    facts: &CanonicalGenerationFacts,
    source: &SourceRevision,
    mut check: impl FnMut() -> Result<(), cartograph_db::rocks::RocksError>,
) -> Result<Option<Vec<cartograph_domain::NormalizedPath>>, cartograph_db::rocks::RocksError> {
    // Source discovery emits ordered paths. Check that contract before using
    // binary search, keeping only the bounded changed paths rather than a
    // second project-wide index of borrowed facts.
    let mut previous = None;
    for (path, _) in &source.admitted_manifest {
        check()?;
        if previous.is_some_and(|prior| prior >= path) {
            return Ok(None);
        }
        previous = Some(path);
    }
    let mut paths = Vec::new();
    for file in facts.files() {
        check()?;
        let Ok(index) = source
            .admitted_manifest
            .binary_search_by(|(path, _)| path.as_str().cmp(file.normalized_path.as_str()))
        else {
            return Ok(None);
        };
        let (path, hash) = &source.admitted_manifest[index];
        if file.content_hash != *hash {
            paths.push(path.clone());
            if paths.len() > 16 {
                return Ok(None);
            }
        }
    }
    Ok((!paths.is_empty()).then_some(paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rocks::{
        Duration, GraphPolicy, NativeBuildProgress, ReadBudget, RocksError, RocksIndexOptions,
        StorageRequest,
    };
    use cartograph_domain::ReferenceKind;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_admission_capacity_falls_back_to_full_before_publication() -> TestResult {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        std::fs::write(&file, "pub fn value() -> u32 { 1 }\n")?;
        let excludes = (0..8)
            .map(|index| format!("unused_{index}_{}/**", "a".repeat(4000)))
            .collect::<Vec<_>>();
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let first = runtime
            .index(
                RocksIndexOptions {
                    additional_excludes: Some(excludes.clone()),
                    ..RocksIndexOptions::default()
                },
                &request,
            )
            .await?;
        // Same-width literals retain the same canonical payload size. Leave
        // room for the exact full payload plus its cloned admission policy,
        // but not the optional sparse change descriptors as well.
        let admission_bytes =
            excludes.len() * size_of::<String>() + excludes.iter().map(String::len).sum::<usize>();
        let publication_policy = GraphPolicy {
            maximum_input_bytes: first
                .native
                .as_ref()
                .ok_or("full build report missing")?
                .modeled_generation_bytes
                + u64::try_from(admission_bytes)?,
            ..GraphPolicy::default()
        };
        let options = RocksIndexOptions::default();
        let progress = NativeBuildProgress::default();
        std::fs::write(&file, "pub fn value() -> u32 { 2 }\n")?;
        let (source, policy) = runtime
            .scan_with_admission(&request, false, excludes.clone(), None)
            .await?;
        let mut warm = runtime.indexing.lock().await;
        let (sparse, _) = runtime
            .try_local_graph(
                &mut warm,
                &GraphBuild {
                    source,
                    policy,
                    options: &options,
                    base: GraphBase::Current(first.revision.generation_id.clone()),
                    request: &request,
                    progress: &progress,
                    excludes: &[],
                    ceiling: None,
                    publication_policy,
                },
            )
            .await?
            .ok_or("sparse input alone should fit")?;
        assert!(matches!(
            sparse.with_source_admission(excludes.clone(), None),
            Err(RocksError::Limit {
                resource: "graph_working_bytes"
            })
        ));
        assert_eq!(
            runtime.current(&request).await?.ok_or("current lost")?.0,
            first.revision
        );
        drop(warm);
        let before = runtime
            .index(
                RocksIndexOptions {
                    force: true,
                    ..RocksIndexOptions::default()
                },
                &request,
            )
            .await?;
        std::fs::write(&file, "pub fn value() -> u32 { 3 }\n")?;
        let (source, policy) = runtime
            .scan_with_admission(&request, false, excludes.clone(), None)
            .await?;
        let mut warm = runtime.indexing.lock().await;
        let prepared = runtime
            .prepare_graph(
                &mut warm,
                GraphBuild {
                    source,
                    policy,
                    options: &options,
                    base: GraphBase::Current(before.revision.generation_id.clone()),
                    request: &request,
                    progress: &NativeBuildProgress::default(),
                    excludes: &excludes,
                    ceiling: None,
                    publication_policy,
                },
            )
            .await?;
        assert_eq!(prepared.incremental_files, 0);
        assert!(prepared.native.is_some());
        assert_eq!(
            runtime.current(&request).await?.ok_or("current lost")?.0,
            before.revision
        );
        drop(warm);
        let published = runtime
            .store
            .publish_graph(prepared.input, &request)
            .await?;
        let status = runtime.status(&request).await?;
        assert!(status.fresh);
        assert_eq!(status.current.ok_or("current lost")?, published.revision);
        assert_eq!(published.revision.run_excludes, excludes);
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_generation_caps_use_full_admission_and_never_publish_over_budget()
    -> TestResult {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        std::fs::write(&file, "pub fn value() -> u32 { 1 }\n")?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        let config = root.path().join(".cartograph/config.json");
        std::fs::write(&config, r#"{"maxGenerationBytes":134217728}"#)?;
        for value in [2, 3] {
            std::fs::write(&file, format!("pub fn value() -> u32 {{ {value} }}\n"))?;
            let bounded = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            assert_eq!(bounded.incremental_files, 0);
            assert!(bounded.native.is_some());
            assert_matches_full(&runtime, &bounded, &request).await?;
        }
        let before = runtime
            .current(&request)
            .await?
            .ok_or("current graph missing")?
            .0;
        // Lowering the setting after warming must also reject a local edit.
        std::fs::write(&config, r#"{"maxGenerationBytes":1}"#)?;
        std::fs::write(&file, "pub fn value() -> u32 { 4 }\n")?;
        assert_capacity_rejects_without_publication(&runtime, &before, &request).await?;
        runtime.shutdown().await?;
        Ok(())
    }

    async fn assert_capacity_rejects_without_publication(
        runtime: &Arc<RocksProjectRuntime>,
        before: &super::super::StoredRevision,
        request: &StorageRequest,
    ) -> TestResult {
        let mut failures = Vec::new();
        for force in [false, true] {
            let failure = runtime
                .index(
                    RocksIndexOptions {
                        force,
                        ..RocksIndexOptions::default()
                    },
                    request,
                )
                .await
                .err()
                .ok_or("over-budget source published")?;
            failures.push(failure.to_string());
            assert_eq!(
                runtime
                    .current(request)
                    .await?
                    .ok_or("current graph lost")?
                    .0,
                *before
            );
        }
        assert_eq!(failures[0], failures[1]);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_changed_path_discovery_stops_before_local_cache_work() -> TestResult
    {
        let root = tempfile::tempdir()?;
        for name in ["first.rs", "second.rs", "third.rs"] {
            std::fs::write(root.path().join(name), "pub fn value() -> u32 { 1 }\n")?;
        }
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(
            root.path().join("first.rs"),
            "pub fn value() -> u32 { 2 }\n",
        )?;
        let (source, policy) = runtime.scan(&request, false).await?;
        let mut warm = runtime.indexing.lock().await;
        let cached = warm.as_ref().ok_or("warm facts missing")?;
        let original_generation = cached.generation.clone();
        let cancelled = StorageRequest::new(Duration::from_secs(60))?;
        let mut checkpoints = 0;
        let outcome = tokio::task::block_in_place(|| {
            discover_local_paths(&cached.facts, &source, || {
                checkpoints += 1;
                if checkpoints == 2 {
                    cancelled.cancel();
                }
                cancelled.check()
            })
        });
        assert!(matches!(outcome, Err(RocksError::Cancelled)));
        assert_eq!(checkpoints, 2);
        let attempted = runtime
            .try_local_graph(
                &mut warm,
                &GraphBuild {
                    source,
                    policy,
                    options: &RocksIndexOptions::default(),
                    base: GraphBase::Current(original_generation.clone()),
                    request: &cancelled,
                    progress: &NativeBuildProgress::default(),
                    excludes: &[],
                    ceiling: None,
                    publication_policy: GraphPolicy::default(),
                },
            )
            .await;
        assert!(matches!(
            attempted,
            Err(RocksProjectError::Storage(RocksError::Cancelled))
        ));
        assert_eq!(
            warm.as_ref().ok_or("warm state consumed")?.generation,
            original_generation
        );
        drop(warm);
        let report = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(report.incremental_files, 1);
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_body_edit_matches_a_cold_full_build_and_retains_topology() -> TestResult {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        std::fs::write(
            &file,
            "pub fn helper() -> u32 { 1 }\npub fn caller() -> u32 { helper() + 1 }\n",
        )?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let before = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(
            &file,
            "pub fn helper() -> u32 { 2 }\npub fn caller() -> u32 { helper() + 9 }\n",
        )?;
        let after = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(after.incremental_files, 1);
        assert!(after.fresh);
        assert_ne!(after.counts.content_digest, before.counts.content_digest);
        assert_eq!(after.counts.edges, before.counts.edges);
        assert!(after.writes.candidate_records < 32);
        let rebuilt = runtime
            .index(
                RocksIndexOptions {
                    force: true,
                    ..RocksIndexOptions::default()
                },
                &request,
            )
            .await?;
        assert_eq!(rebuilt.incremental_files, 0);
        assert_eq!(after.counts, rebuilt.counts);
        assert_eq!(
            after.revision.source_revision,
            rebuilt.revision.source_revision
        );
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn polyglot_body_edits_match_cold_reconciliation() -> TestResult {
        for (extension, before, after) in [
            (
                "py",
                "def calculate():\n    return 1\n",
                "def calculate():\n    return 2\n",
            ),
            (
                "ts",
                "export function calculate() { return 1; }\n",
                "export function calculate() { return 2; }\n",
            ),
            (
                "go",
                "package sample\nfunc Calculate() int { return 1 }\n",
                "package sample\nfunc Calculate() int { return 2 }\n",
            ),
        ] {
            let root = tempfile::tempdir()?;
            let file = root.path().join(format!("sample.{extension}"));
            std::fs::write(&file, before)?;
            let request = StorageRequest::new(Duration::from_secs(60))?;
            let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
            runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            std::fs::write(&file, after)?;
            let edited = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            assert_eq!(
                edited.incremental_files, 1,
                "{extension} did not use the proven local edit path"
            );
            assert_matches_full(&runtime, &edited, &request).await?;
            runtime.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn xml_template_role_edits_reconcile_callers_in_other_files() -> TestResult {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target.xml");
        let original = "<mapper namespace=\"example.Target\"><sql    id=\"cols\"/></mapper>\n";
        std::fs::write(&target, original)?;
        std::fs::write(
            root.path().join("caller.xml"),
            "<mapper namespace=\"example.Caller\"><select id=\"query\"><include refid=\"example.Target.cols\"/></select></mapper>\n",
        )?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let before = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(&target, original.replace("sql   ", "select"))?;
        let edited = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(edited.incremental_files, 0);
        assert_eq!(edited.counts.edges + 1, before.counts.edges);
        assert_matches_full(&runtime, &edited, &request).await?;
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn declaration_addition_and_deletion_reconcile_previously_unresolved_callers()
    -> TestResult {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        std::fs::write(&file, "pub fn caller() { later(); }\n")?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(&file, "pub fn caller() { later(); }\npub fn later() {}\n")?;
        let added = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(added.incremental_files, 0);
        runtime
            .store
            .read(
                Some(added.revision.generation_id),
                ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?,
                &request,
                |snapshot, budget| {
                    let graph = snapshot.graph(budget)?;
                    let (files, _) = graph.files(None, 10, budget)?;
                    let file = files.first().ok_or(RocksError::Format)?;
                    let (rows, _) = graph.references(&file.file_id, None, 100, budget)?;
                    assert!(rows.iter().any(|row| row.reference_name == "later"
                        && row.reference_kind == ReferenceKind::Calls.as_str()
                        && row.target_symbol_id.is_some()));
                    Ok(())
                },
            )
            .await?;
        std::fs::write(&file, "pub fn caller() { later(); }\n")?;
        let deleted = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(deleted.incremental_files, 0);
        runtime
            .store
            .read(
                Some(deleted.revision.generation_id),
                ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?,
                &request,
                |snapshot, budget| {
                    let graph = snapshot.graph(budget)?;
                    let (files, _) = graph.files(None, 10, budget)?;
                    let file = files.first().ok_or(RocksError::Format)?;
                    let (rows, _) = graph.references(&file.file_id, None, 100, budget)?;
                    assert!(
                        rows.iter()
                            .any(|row| row.reference_name == "later"
                                && row.target_symbol_id.is_none())
                    );
                    Ok(())
                },
            )
            .await?;
        runtime.shutdown().await?;
        Ok(())
    }
    async fn assert_matches_full(
        runtime: &Arc<RocksProjectRuntime>,
        edited: &super::super::RocksIndexReport,
        request: &StorageRequest,
    ) -> TestResult {
        let rebuilt = runtime
            .index(
                RocksIndexOptions {
                    force: true,
                    ..RocksIndexOptions::default()
                },
                request,
            )
            .await?;
        assert_eq!(edited.counts, rebuilt.counts);
        assert_eq!(
            edited.revision.source_revision,
            rebuilt.revision.source_revision
        );
        assert!(edited.fresh && rebuilt.fresh);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn numerical_and_docstring_edits_in_multiple_files_match_full_reconciliation()
    -> TestResult {
        let root = tempfile::tempdir()?;
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        std::fs::write(
            &first,
            "/// old docs\npub fn first(value: u32) -> u32 { value / 1 }\n",
        )?;
        std::fs::write(&second, "pub fn second() -> u32 { 4 + 1 }\n")?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(
            &first,
            "/// new docs\npub fn first(value: u32) -> u32 { value / 0 }\n",
        )?;
        std::fs::write(&second, "pub fn second() -> u32 { 5 + 1 }\n")?;
        let edited = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(edited.incremental_files, 2);
        assert_matches_full(&runtime, &edited, &request).await?;
        runtime.shutdown().await?;
        Ok(())
    }

    fn large_function(value: u32) -> String {
        format!(
            "pub fn calculate(input: u32) -> u32 {{\n    let total = input + {value};\n    let scale = total * 2;\n    let extra = scale + 3;\n    let other = extra * 4;\n    other + total\n}}\n"
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn literal_edits_preserve_normalized_clones_and_exact_clone_changes_fall_back()
    -> TestResult {
        let root = tempfile::tempdir()?;
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        std::fs::write(&first, large_function(1))?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(&first, large_function(2))?;
        let edited = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(edited.incremental_files, 1);
        assert_matches_full(&runtime, &edited, &request).await?;
        std::fs::write(&second, large_function(1))?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        for value in [1, 2] {
            std::fs::write(&first, large_function(value))?;
            let edited = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            assert_eq!(
                edited.incremental_files, 0,
                "creating or breaking an exact clone must reconcile peers"
            );
            assert_matches_full(&runtime, &edited, &request).await?;
        }
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changed_call_target_reconciles_edges_and_references() -> TestResult {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        let original = "pub fn alpha() {}\npub fn bravo() {}\npub fn caller() { alpha(); }\n";
        std::fs::write(&file, original)?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let before = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(&file, original.replace("{ alpha(); }", "{ bravo(); }"))?;
        let edited = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(edited.incremental_files, 0);
        assert_ne!(edited.counts.content_digest, before.counts.content_digest);
        assert_matches_full(&runtime, &edited, &request).await?;
        runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adding_and_deleting_a_file_reconciles_callers_in_unchanged_files() -> TestResult {
        let root = tempfile::tempdir()?;
        std::fs::write(
            root.path().join("lib.rs"),
            "mod later;\npub fn caller() { crate::later::target(); }\n",
        )?;
        let later = root.path().join("later.rs");
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let before = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(&later, "pub fn target() {}\n")?;
        let added = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(added.incremental_files, 0);
        assert!(added.counts.edges > before.counts.edges);
        assert_matches_full(&runtime, &added, &request).await?;
        std::fs::remove_file(&later)?;
        let deleted = runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        assert_eq!(deleted.incremental_files, 0);
        assert_eq!(deleted.counts, before.counts);
        assert_matches_full(&runtime, &deleted, &request).await?;
        runtime.shutdown().await?;
        Ok(())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_or_corrupt_cached_extraction_selects_a_complete_build() -> TestResult {
        use cartograph_db::{NativeParseCacheKey, NativeParseCacheKeyInput};
        use cartograph_domain::{NormalizedPath, SourceLanguage};
        for corrupt in [false, true] {
            let root = tempfile::tempdir()?;
            let file = root.path().join("lib.rs");
            std::fs::write(&file, "pub fn value() -> u32 { 1 }\n")?;
            let request = StorageRequest::new(Duration::from_secs(60))?;
            let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
            let before = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            let old_file = {
                let warm = runtime.indexing.lock().await;
                warm.as_ref()
                    .ok_or("warm base missing")?
                    .facts
                    .files()
                    .first()
                    .ok_or("file missing")?
                    .clone()
            };
            let key = NativeParseCacheKey::new(NativeParseCacheKeyInput {
                project_id: runtime.database.project_id().clone(),
                extractor_contract_digest: before.parse_cache_contract_digest,
                path: NormalizedPath::parse("lib.rs")?,
                language: SourceLanguage::Rust,
                content_hash: old_file.content_hash,
                source_bytes: old_file.byte_size,
            });
            assert!(runtime.database.evict_native_parse_cache(&key).await?);
            if corrupt {
                runtime
                    .database
                    .store_native_parse_cache(&key, b"malformed extraction")
                    .await?;
            }
            std::fs::write(&file, "pub fn value() -> u32 { 2 }\n")?;
            let edited = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            assert_eq!(edited.incremental_files, 0);
            assert_matches_full(&runtime, &edited, &request).await?;
            runtime.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extraction_policy_span_and_partial_parse_changes_use_full_reconciliation() -> TestResult
    {
        let root = tempfile::tempdir()?;
        let file = root.path().join("lib.rs");
        std::fs::write(&file, "/// docs\npub fn value() -> u32 { 1 }\n")?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        runtime
            .index(RocksIndexOptions::default(), &request)
            .await?;
        std::fs::write(
            root.path().join(".cartograph/config.json"),
            r#"{"extractDocstrings":false}"#,
        )?;
        for text in [
            "/// docs\npub fn value() -> u32 { 2 }\n",
            "/// docs\npub fn value() -> u32 {\n    2\n}\n",
            "pub fn value() -> u32 {\n",
        ] {
            std::fs::write(&file, text)?;
            let edited = runtime
                .index(RocksIndexOptions::default(), &request)
                .await?;
            assert_eq!(edited.incremental_files, 0);
            assert_matches_full(&runtime, &edited, &request).await?;
        }
        runtime.shutdown().await?;
        Ok(())
    }
}
