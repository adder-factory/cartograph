use std::{
    fmt::Write as _,
    sync::atomic::{AtomicUsize, Ordering},
    time::SystemTime,
};

use cartograph_config::DatabaseSettings;
use cartograph_search::{ContextBudget, ContextRequest, ContextRequestOptions, IndexFreshness};
use cartograph_test_support::TestSchemaGuard;

use super::*;

fn symbol() -> SymbolId {
    SymbolId::parse("11111111-1111-4111-8111-111111111111").unwrap_or_else(|e| panic!("id: {e}"))
}

#[test]
fn offered_actions_cannot_repeat_or_use_unobserved_provider_paths() {
    let action = Action::Read { symbol: symbol() };
    let used = BTreeSet::from([action_key(&action)]);
    let mut actions = AvailableActions {
        offered: BTreeMap::new(),
        used: &used,
    };
    actions.add("read_0".to_owned(), action, "read".to_owned());
    assert!(actions.offered.is_empty());
    actions.add(
        "outline_0".to_owned(),
        Action::Outline {
            path: "src/a.rs".to_owned(),
        },
        "outline".to_owned(),
    );
    actions.add(
        "outline_1".to_owned(),
        Action::Outline {
            path: "src/a.rs".to_owned(),
        },
        "outline duplicate".to_owned(),
    );
    assert_eq!(actions.offered.len(), 1);
    let decision = response("../../secret", &BTreeMap::new());
    assert!(selected_step(&actions.offered, &decision, 0).is_none());
    assert_eq!(
        query_identifiers("find calculate_total and crate::parse while ignoring normal prose"),
        ["calculate_total", "crate::parse"]
    );
}

#[test]
fn candidate_admission_bounds_bytes_and_deduplicates_identity() {
    let mut report = NavigationReport::new(NavigationStop::NotConfigured, None);
    let candidate = NavigationCandidate {
        symbol_id: symbol(),
        path: "src/a.rs".to_owned(),
        name: "parse".to_owned(),
        symbol_kind: "function".to_owned(),
        signature: bounded_signature(&"é".repeat(200)),
        start_line: Some(1),
        end_line: Some(2),
        relevance: None,
    };
    assert!(candidate.signature.len() <= SIGNATURE_TEXT_LIMIT);
    let mut excessive = candidate.clone();
    excessive.name = "n".repeat(513);
    admit_candidate(&mut report, excessive);
    assert!(report.candidates_truncated && report.candidates.is_empty());
    admit_candidate(&mut report, candidate.clone());
    admit_candidate(&mut report, candidate);
    assert_eq!(report.candidates.len(), 1);
}

fn response(choice: &str, criteria: &BTreeMap<String, String>) -> JevDecision {
    JevDecision {
        model: JEV_MODEL.to_owned(),
        answers: BTreeMap::from([
            (
                "next".to_owned(),
                JevAnswer::Choice {
                    choice: choice.to_owned(),
                    confidence: 0.8,
                    probabilities: criteria
                        .keys()
                        .map(|key| (key.clone(), if key == choice { 1.0 } else { 0.0 }))
                        .collect(),
                },
            ),
            ("sufficient".to_owned(), JevAnswer::Noul { noul: 0.2 }),
        ]),
    }
}

struct Scripted {
    calls: AtomicUsize,
    error_after_read: bool,
}

impl DecisionProvider for Scripted {
    fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        assert!(
            questions.contains_key("next") && questions.contains_key("sufficient"),
            "every independent decision shares one request"
        );
        assert!(
            questions
                .keys()
                .all(|key| key == "next" || key == "sufficient" || key.starts_with("relevant_")),
            "only bounded relevance questions accompany the round decisions"
        );
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 1 && self.error_after_read {
            return std::future::ready(Err(JevError::RateLimited));
        }
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        let choice = if call == 0 {
            "read_0"
        } else if call == 1 {
            "callees_0"
        } else {
            assert!(
                state["sourceWindows"]
                    .as_array()
                    .is_some_and(|sources| !sources.is_empty())
            );
            "finish"
        };
        assert!(criteria.contains_key(choice));
        std::future::ready(Ok(response(choice, criteria)))
    }
}

struct Pending;

struct NativeEvidenceProbe;
impl DecisionProvider for NativeEvidenceProbe {
    fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        let sources = state["nativeSourceWindows"]
            .as_array()
            .unwrap_or_else(|| panic!("native windows"));
        assert_eq!(sources.len(), 1);
        assert!(
            sources[0]["excerpt"]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("native text"))
                .contains("entry_point")
        );
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice");
        };
        let candidates = state["candidates"]
            .as_array()
            .unwrap_or_else(|| panic!("candidates"));
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate["symbolKind"] == "constant")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate["symbolKind"] == "struct")
        );
        for (index, candidate) in candidates.iter().enumerate() {
            assert!(candidate["startLine"].as_u64().is_some());
            assert!(candidate["endLine"].as_u64().is_some());
            if !matches!(
                candidate["symbolKind"].as_str(),
                Some("function" | "method")
            ) {
                assert!(!criteria.contains_key(&format!("callers_{index}")));
                assert!(!criteria.contains_key(&format!("callees_{index}")));
            }
            if candidate["symbolId"] == sources[0]["symbol"]["symbol_id"] {
                assert!(!criteria.contains_key(&format!("read_{index}")));
            }
        }
        std::future::ready(Ok(response("finish", criteria)))
    }
}

impl DecisionProvider for Pending {
    async fn decide(
        &self,
        _: &Value,
        _: &BTreeMap<String, JevQuestion>,
    ) -> Result<JevDecision, JevError> {
        std::future::pending().await
    }
}

struct Terminal(&'static str);
impl DecisionProvider for Terminal {
    fn decide(
        &self,
        _: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        std::future::ready(Ok(response(self.0, criteria)))
    }
}

struct ExhaustBudget;
impl DecisionProvider for ExhaustBudget {
    fn decide(
        &self,
        _: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        let choice = criteria
            .keys()
            .find(|key| *key != "finish" && *key != "abstain")
            .unwrap_or_else(|| panic!("fixture requires at least seven distinct operations"));
        std::future::ready(Ok(response(choice, criteria)))
    }
}

struct ChangedSource<'a> {
    root: &'a std::path::Path,
}
impl DecisionProvider for ChangedSource<'_> {
    fn decide(
        &self,
        _: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        std::fs::write(self.root.join("lib.rs"), "pub fn changed_source() {}\n")
            .unwrap_or_else(|e| panic!("change fixture: {e}"));
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        std::future::ready(Ok(response("finish", criteria)))
    }
}

/// Judges every candidate relevant in the first round and declares the read
/// source sufficient in the second, so one fan-out round replaces several
/// sequential read decisions.
struct FanOut {
    calls: AtomicUsize,
    judged: AtomicUsize,
}
impl DecisionProvider for FanOut {
    fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        let relevance = questions
            .keys()
            .filter(|key| key.starts_with("relevant_"))
            .cloned()
            .collect::<Vec<_>>();
        let mut decision = response("finish", criteria);
        if call == 0 {
            let candidates = state["candidates"]
                .as_array()
                .map_or(0, Vec::len)
                .min(MAXIMUM_RELEVANCE_QUESTIONS);
            assert!(candidates > 0);
            assert_eq!(
                relevance.len(),
                candidates,
                "every unread candidate is judged"
            );
            self.judged.store(candidates, Ordering::SeqCst);
            for key in relevance {
                decision.answers.insert(key, JevAnswer::Noul { noul: 0.9 });
            }
        } else {
            assert_eq!(
                state["sourceWindows"].as_array().map_or(0, Vec::len),
                self.judged
                    .load(Ordering::SeqCst)
                    .min(MAXIMUM_READS_PER_ROUND),
                "the second round sees every source read in the first"
            );
            decision
                .answers
                .insert("sufficient".to_owned(), JevAnswer::Noul { noul: 0.95 });
            for key in relevance {
                decision.answers.insert(key, JevAnswer::Noul { noul: 0.1 });
            }
        }
        std::future::ready(Ok(decision))
    }
}

/// Declares the native source sufficient while still naming a read, which must
/// not execute once sufficiency clears the stop threshold.
struct AlreadySufficient;
impl DecisionProvider for AlreadySufficient {
    fn decide(
        &self,
        _: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> impl Future<Output = Result<JevDecision, JevError>> {
        let Some(JevQuestion::Choice { criteria, .. }) = questions.get("next") else {
            panic!("choice question missing");
        };
        let read = criteria
            .keys()
            .find(|key| key.starts_with("read_"))
            .unwrap_or_else(|| panic!("fixture offers a read"))
            .clone();
        let mut decision = response(&read, criteria);
        decision
            .answers
            .insert("sufficient".to_owned(), JevAnswer::Noul { noul: 0.9 });
        std::future::ready(Ok(decision))
    }
}

fn navigator<'a>(
    runtime: &'a ProjectRuntime,
    project: &'a ProjectId,
    packet: &'a ContextPacket,
) -> Navigator<'a> {
    let request = NavigationRequest::new(project, "find entry_point implementation", packet)
        .unwrap_or_else(|e| panic!("request: {e}"));
    Navigator::new(runtime, request, ProjectCancellation::new())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector; provider decisions are scripted"]
async fn live_navigation_preserves_evidence_on_outage_and_fences_cancellation_source_and_generation()
 {
    let url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("test database not configured"));
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_navigation_{}_{}", std::process::id(), stamp);
    let guard = TestSchemaGuard::new(&url, &schema).unwrap_or_else(|e| panic!("guard: {e}"));
    let settings = DatabaseSettings::parse(&url, Some("4"), Some("10000"))
        .and_then(|s| s.with_schema(&schema))
        .unwrap_or_else(|e| panic!("settings: {e}"));
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    std::fs::write(
        root.path().join("lib.rs"),
        "pub fn entry_point() -> u32 { helper() }\nfn helper() -> u32 { 42 }\nconst LIMIT: u32 = 7;\nstruct Settings { value: u32 }\n",
    )
    .unwrap_or_else(|e| panic!("source: {e}"));
    let runtime = ProjectRuntime::connect(root.path(), &settings)
        .await
        .unwrap_or_else(|e| panic!("runtime: {e}"));
    runtime
        .index(crate::IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|e| panic!("index: {e}"));
    let status = runtime
        .status()
        .await
        .unwrap_or_else(|e| panic!("status: {e}"));
    let project = status
        .snapshot
        .unwrap_or_else(|| panic!("snapshot"))
        .project_id;
    let request = ContextRequest::new(
        project.clone(),
        "entry_point",
        ContextRequestOptions::new(IndexFreshness::Current, ContextBudget::default()),
    )
    .and_then(|r| {
        r.with_anchor(cartograph_search::ContextAnchor::ExactName(
            "entry_point".to_owned(),
        ))
    })
    .unwrap_or_else(|e| panic!("context: {e}"));
    let packet = DeterministicRetriever::new(runtime.database.clone())
        .context_packet(&request)
        .await
        .unwrap_or_else(|e| panic!("packet: {e}"));
    verify_native_and_outage(&runtime, &project, &packet).await;
    verify_fan_out_round(&runtime, &project, &packet).await;
    verify_stop_conditions(&runtime, &project, &packet).await;
    verify_cancel_and_deadline(&runtime, &project, &packet).await;
    verify_native_evidence_and_candidate_kinds(&runtime, &project, &packet).await;
    let mut nav = navigator(&runtime, &project, &packet);
    let changed = nav.run_bounded(&ChangedSource { root: root.path() }).await;
    assert!(
        matches!(changed, Err(ProjectError::SourceContextUnavailable)),
        "changed source result: {changed:?}"
    );
    runtime
        .index(crate::IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|e| panic!("new generation: {e}"));
    let mut nav = navigator(&runtime, &project, &packet);
    assert!(matches!(
        nav.run_bounded(&Pending).await,
        Err(ProjectError::SourceContextUnavailable)
    ));
    verify_truncation(&runtime, root.path(), &project).await;
    runtime.close().await;
    guard
        .cleanup()
        .await
        .unwrap_or_else(|e| panic!("cleanup: {e}"));
}

async fn verify_fan_out_round(
    runtime: &ProjectRuntime,
    project: &ProjectId,
    packet: &ContextPacket,
) {
    let provider = FanOut {
        calls: AtomicUsize::new(0),
        judged: AtomicUsize::new(0),
    };
    let mut nav = navigator(runtime, project, packet);
    nav.run_bounded(&provider)
        .await
        .unwrap_or_else(|e| panic!("fan-out navigation: {e}"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(nav.report.stop, NavigationStop::Finished);
    let reads = nav
        .report
        .decisions
        .iter()
        .filter(|step| matches!(step.action, Action::Read { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        reads.len(),
        provider
            .judged
            .load(Ordering::SeqCst)
            .min(MAXIMUM_READS_PER_ROUND)
    );
    assert!(reads.iter().all(|step| step.round == 0 && step.completed));
    assert_eq!(nav.report.source_windows.len(), reads.len());
    assert_eq!(nav.operations, reads.len());
    assert!(
        nav.report
            .candidates
            .iter()
            .all(|candidate| candidate.relevance.is_some())
    );
}

async fn verify_native_evidence_and_candidate_kinds(
    runtime: &ProjectRuntime,
    project: &ProjectId,
    packet: &ContextPacket,
) {
    let root = packet.evidence()[0]
        .symbol_id()
        .unwrap_or_else(|| panic!("root symbol"))
        .clone();
    let sources = runtime
        .source_context_batch_with_cancellation(
            packet
                .generation()
                .unwrap_or_else(|| panic!("generation"))
                .generation_id(),
            vec![SourceContextRequest::new(
                root,
                SourceContextOptions::new(12, 4096)
                    .unwrap_or_else(|error| panic!("bounds: {error}")),
            )],
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("native source: {error}"));
    let request = ContextRequest::new(
        project.clone(),
        "entry_point",
        ContextRequestOptions::new(IndexFreshness::Current, ContextBudget::default()),
    )
    .and_then(|request| {
        request.with_anchor(cartograph_search::ContextAnchor::ExactPath(
            NormalizedPath::parse("lib.rs").unwrap_or_else(|error| panic!("path: {error}")),
        ))
    })
    .unwrap_or_else(|error| panic!("outline request: {error}"));
    let packet = DeterministicRetriever::new(runtime.database.clone())
        .context_packet(&request)
        .await
        .unwrap_or_else(|error| panic!("outline packet: {error}"));
    let request = NavigationRequest::new(project, "find entry_point implementation", &packet)
        .unwrap_or_else(|error| panic!("navigation: {error}"))
        .with_native_sources(&sources)
        .unwrap_or_else(|error| panic!("native evidence: {error}"));
    let mut sufficient = Navigator::new(
        runtime,
        NavigationRequest::new(project, "find entry_point implementation", &packet)
            .and_then(|request| request.with_native_sources(&sources))
            .unwrap_or_else(|error| panic!("sufficient request: {error}")),
        ProjectCancellation::new(),
    );
    sufficient
        .run_bounded(&AlreadySufficient)
        .await
        .unwrap_or_else(|error| panic!("sufficient navigation: {error}"));
    assert_eq!(sufficient.report.stop, NavigationStop::Finished);
    assert_eq!(sufficient.operations, 0);
    assert!(
        sufficient
            .report
            .decisions
            .iter()
            .all(|step| matches!(step.action, Action::Finish) && step.completed),
        "an unexecuted chosen operation must not be reported"
    );
    assert!(sufficient.report.source_windows.is_empty());
    let mut nav = Navigator::new(runtime, request, ProjectCancellation::new());
    nav.run_bounded(&NativeEvidenceProbe)
        .await
        .unwrap_or_else(|error| panic!("seeded navigation: {error}"));
    assert_eq!(nav.report.stop, NavigationStop::Finished);
    assert!(
        nav.report.source_windows.is_empty(),
        "native evidence must not be returned twice"
    );
    assert_eq!(nav.report.native_source_windows, 1);
}

async fn verify_native_and_outage(
    runtime: &ProjectRuntime,
    project: &ProjectId,
    packet: &ContextPacket,
) {
    for (policy, stop) in [
        (NavigationPolicy::Auto, NavigationStop::NotConfigured),
        (NavigationPolicy::Native, NavigationStop::NativeRequested),
        (NavigationPolicy::Summary, NavigationStop::SummaryRequested),
    ] {
        let request = NavigationRequest::new(project, "entry_point", packet)
            .unwrap_or_else(|e| panic!("request: {e}"))
            .with_policy(policy);
        let result = runtime
            .navigate(request, ProjectCancellation::new())
            .await
            .unwrap_or_else(|e| panic!("native: {e}"));
        assert_eq!(result.stop, stop);
        assert!(result.source_windows.is_empty());
    }
    for error_after_read in [false, true] {
        let mut nav = navigator(runtime, project, packet);
        nav.run_bounded(&Scripted {
            calls: AtomicUsize::new(0),
            error_after_read,
        })
        .await
        .unwrap_or_else(|e| panic!("navigation: {e}"));
        assert_eq!(nav.report.source_windows.len(), 1);
        let text = nav.report.source_windows[0]
            .excerpt()
            .unwrap_or_else(|| panic!("source excerpt"))
            .text();
        assert!(text.contains("pub fn entry_point() -> u32 { helper() }"));
        assert!(nav.report.source_windows[0].fresh());
        if error_after_read {
            assert_eq!(nav.report.stop, NavigationStop::ProviderUnavailable);
            assert_eq!(nav.report.provider_error, Some(JevError::RateLimited));
        } else {
            assert_eq!(nav.report.stop, NavigationStop::Finished);
            assert!(
                nav.report
                    .candidates
                    .iter()
                    .any(|candidate| candidate.name == "helper")
            );
        }
    }
}

const SHORT_DEADLINE: Duration = Duration::from_millis(25);

async fn verify_cancel_and_deadline(
    runtime: &ProjectRuntime,
    project: &ProjectId,
    packet: &ContextPacket,
) {
    let mut nav = navigator(runtime, project, packet);
    assert!(matches!(
        nav.run_with_deadline(&Pending, SHORT_DEADLINE).await,
        Err(ProjectError::SourceContextUnavailable)
    ));
    let mut nav = navigator(runtime, project, packet);
    let cancellation = nav.caller_cancellation.clone();
    let cancel = async {
        tokio::time::sleep(SHORT_DEADLINE).await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(nav.run_bounded(&Pending), cancel);
    assert!(matches!(result, Err(ProjectError::RequestCancelled)));

    let held = runtime
        .source_scan_permits
        .clone()
        .acquire_owned()
        .await
        .unwrap_or_else(|e| panic!("scan admission: {e}"));
    let observations = runtime.source_scan_observations.load(Ordering::Relaxed);
    let mut nav = navigator(runtime, project, packet);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        nav.run_with_deadline(&Pending, SHORT_DEADLINE),
    )
    .await;
    assert!(matches!(
        result,
        Ok(Err(ProjectError::SourceContextUnavailable))
    ));
    assert!(nav.cancellation.is_cancelled());
    assert!(!nav.caller_cancellation.is_cancelled());
    assert_eq!(
        runtime.source_scan_observations.load(Ordering::Relaxed),
        observations
    );
    drop(held);
    assert_eq!(runtime.source_scan_permits.available_permits(), 1);
}

async fn verify_stop_conditions(
    runtime: &ProjectRuntime,
    project: &ProjectId,
    packet: &ContextPacket,
) {
    for (choice, expected) in [
        ("abstain", NavigationStop::Abstained),
        ("finish", NavigationStop::Abstained),
        ("not_offered", NavigationStop::ProviderUnavailable),
    ] {
        let mut nav = navigator(runtime, project, packet);
        nav.run_bounded(&Terminal(choice))
            .await
            .unwrap_or_else(|e| panic!("terminal decision: {e}"));
        assert_eq!(nav.report.stop, expected);
        assert!(nav.report.source_windows.is_empty());
        if choice == "not_offered" {
            assert_eq!(nav.report.provider_error, Some(JevError::InvalidResponse));
        }
    }
    let mut nav = navigator(runtime, project, packet);
    nav.run_bounded(&ExhaustBudget)
        .await
        .unwrap_or_else(|e| panic!("bounded exploration: {e}"));
    assert_eq!(nav.report.stop, NavigationStop::StepLimit);
    assert_eq!(nav.report.decisions.len(), 7);
    assert_eq!(
        nav.used.len(),
        7,
        "repeated operations must not consume the budget"
    );
    for task in [
        String::new(),
        "invalid\0task".to_owned(),
        "x".repeat(cartograph_search::CONTEXT_QUERY_MAXIMUM_BYTES + 1),
    ] {
        assert!(matches!(
            NavigationRequest::new(project, &task, packet),
            Err(ProjectError::InvalidOptions)
        ));
    }
    std::fs::create_dir_all(runtime.root.join(".cartograph"))
        .unwrap_or_else(|e| panic!("config directory: {e}"));
    cartograph_llm::write_project_llm_tiers(
        &runtime.root,
        &[cartograph_llm::ProjectLlmTierInput::jev(
            "CARTOGRAPH_TEST_INTENTIONALLY_MISSING_JEV_KEY",
        )
        .unwrap_or_else(|e| panic!("tier: {e}"))],
    )
    .unwrap_or_else(|e| panic!("configuration: {e}"));
    let request = NavigationRequest::new(project, "entry_point", packet)
        .unwrap_or_else(|e| panic!("request: {e}"));
    let result = runtime
        .navigate(request, ProjectCancellation::new())
        .await
        .unwrap_or_else(|e| panic!("native fallback: {e}"));
    assert_eq!(result.stop, NavigationStop::ProviderUnavailable);
    assert_eq!(
        result.provider_error,
        Some(JevError::CredentialMissing {
            environment_variable: "CARTOGRAPH_TEST_INTENTIONALLY_MISSING_JEV_KEY".to_owned(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_cancels_and_joins_an_active_source_worker_before_returning() {
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let worker_permits = permits.clone();
    let cancellation = ProjectCancellation::new();
    let worker_cancellation = cancellation.clone();
    let caller = ProjectCancellation::new();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (observed, cancelled) = tokio::sync::oneshot::channel();
    let (release, completion) = std::sync::mpsc::channel();
    let operation = async move {
        let permit = worker_permits
            .acquire_owned()
            .await
            .map_err(|_| ProjectError::SourceScanFailed)?;
        crate::run_source_worker(move || {
            let _permit = permit;
            let _ = started.send(());
            while !worker_cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = observed.send(());
            completion
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| ProjectError::SourceScanFailed)?;
            Err::<(), _>(ProjectError::RequestCancelled)
        })
        .await
    };
    let task = tokio::spawn(
        NavigationDeadline {
            cancellation,
            caller: caller.clone(),
            duration: Duration::from_millis(50),
        }
        .complete(operation),
    );
    ready.await.unwrap_or_else(|e| panic!("worker start: {e}"));
    cancelled
        .await
        .unwrap_or_else(|e| panic!("worker cancellation: {e}"));
    assert!(!task.is_finished(), "deadline must join the active worker");
    assert_eq!(permits.available_permits(), 0);
    release
        .send(())
        .unwrap_or_else(|e| panic!("worker release: {e}"));
    assert!(matches!(
        task.await,
        Ok(Err(ProjectError::SourceContextUnavailable))
    ));
    assert_eq!(permits.available_permits(), 1);
    assert!(!caller.is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_navigation_signals_the_active_source_worker() {
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let worker_permits = permits.clone();
    let cancellation = ProjectCancellation::new();
    let worker_cancellation = cancellation.clone();
    let caller = ProjectCancellation::new();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let operation = async move {
        let permit = worker_permits
            .acquire_owned()
            .await
            .map_err(|_| ProjectError::SourceScanFailed)?;
        crate::run_source_worker(move || {
            let _ = started.send(());
            let limit = std::time::Instant::now() + Duration::from_secs(2);
            while !worker_cancellation.is_cancelled() && std::time::Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(1));
            }
            let observed = worker_cancellation.is_cancelled();
            drop(permit);
            let _ = finished.send(observed);
            Err::<(), _>(ProjectError::RequestCancelled)
        })
        .await
    };
    let task = tokio::spawn(
        NavigationDeadline {
            cancellation,
            caller: caller.clone(),
            duration: Duration::from_secs(30),
        }
        .complete(operation),
    );
    ready.await.unwrap_or_else(|e| panic!("worker start: {e}"));
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
    assert!(
        done.await
            .unwrap_or_else(|e| panic!("worker completion: {e}"))
    );
    assert_eq!(permits.available_permits(), 1);
    assert!(!caller.is_cancelled());
}

const FIXTURE_DECLARATIONS: usize = 30;
const FIXTURE_RETRIEVAL_LIMIT: u16 = 40;
const EXPECTED_SEED_CANDIDATES: usize = 12;
const EXPECTED_LOOKUP_CANDIDATES: usize = 24;

async fn verify_truncation(runtime: &ProjectRuntime, root: &std::path::Path, project: &ProjectId) {
    let mut outline = String::new();
    for index in 0..FIXTURE_DECLARATIONS {
        std::fs::write(
            root.join(format!("duplicate_{index}.rs")),
            "pub fn repeated_name() {}\n",
        )
        .unwrap_or_else(|e| panic!("duplicate declaration fixture: {e}"));
        writeln!(outline, "pub fn item_{index}() {{}}")
            .unwrap_or_else(|e| panic!("outline fixture: {e}"));
    }
    std::fs::write(root.join("outline.rs"), outline).unwrap_or_else(|e| panic!("outline: {e}"));
    runtime
        .index(crate::IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|e| panic!("truncation index: {e}"));
    let budget = ContextBudget::new(cartograph_search::ContextBudgetInput {
        candidate_limit: FIXTURE_RETRIEVAL_LIMIT,
        exact_limit: FIXTURE_RETRIEVAL_LIMIT,
        evidence_limit: FIXTURE_RETRIEVAL_LIMIT,
        ..Default::default()
    })
    .unwrap_or_else(|e| panic!("budget: {e}"));
    let request = ContextRequest::new(
        project.clone(),
        "repeated_name",
        ContextRequestOptions::new(IndexFreshness::Current, budget),
    )
    .and_then(|r| {
        r.with_anchor(cartograph_search::ContextAnchor::ExactName(
            "repeated_name".to_owned(),
        ))
    })
    .unwrap_or_else(|e| panic!("request: {e}"));
    let packet = DeterministicRetriever::new(runtime.database.clone())
        .context_packet(&request)
        .await
        .unwrap_or_else(|e| panic!("packet: {e}"));
    assert!(
        packet
            .evidence()
            .iter()
            .filter(|e| e.symbol_id().is_some())
            .count()
            > EXPECTED_SEED_CANDIDATES
    );
    let nav = navigator(runtime, project, &packet);
    assert_eq!(nav.report.candidates.len(), EXPECTED_SEED_CANDIDATES);
    assert!(nav.report.candidates_truncated);
    for action in [
        Action::Outline {
            path: "outline.rs".to_owned(),
        },
        Action::ExactName {
            name: "repeated_name".to_owned(),
        },
    ] {
        let mut nav = navigator(runtime, project, &packet);
        nav.report.candidates.clear();
        nav.report.candidates_truncated = false;
        nav.execute(&action)
            .await
            .unwrap_or_else(|e| panic!("bounded lookup: {e}"));
        assert!(nav.report.candidates_truncated);
        assert_eq!(nav.report.candidates.len(), EXPECTED_LOOKUP_CANDIDATES);
    }
}
