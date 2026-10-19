//! Integration coverage for Cartograph project-runtime and agent evidence contracts.

mod dependency_ownership;

use std::{collections::BTreeSet, fs, path::Path, process::Stdio, time::Duration};

use cartograph_agent::{IndexOptions, ProjectRuntime, ReviewOptions};
use cartograph_db::CurrentSymbolSetLookup;
use cartograph_search::{
    ContextAbstention, ContextBudget, ContextPacket, ContextRequest, DeterministicRetriever,
    EvidenceItem, EvidenceReason, IndexFreshness, RetrievalConfidence, ReviewAbstention,
    ReviewPacket, TraversalBudget,
};
use tokio::process::{Child, Command};

#[path = "patch_task_evaluation/fixture.rs"]
mod fixture;

const TOP_K: usize = 5;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const BASELINE_EDIT_PRECISION: f64 = 0.866_666_666_666_666_8;
const BASELINE_ESTIMATED_TOKENS: f64 = 982.0;
const SCORE_TOLERANCE: f64 = 1.0e-12;

fn score_matches(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= SCORE_TOLERANCE
}

type EvalResult<T> = Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RankedSymbol {
    name: String,
    path: String,
    rank: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CaseObservation {
    case_id: &'static str,
    ranked_symbols: Vec<RankedSymbol>,
    predicted_edit_files: Vec<String>,
    selected_test_files: Vec<String>,
    context_selected_test_files: Vec<String>,
    review_selected_test_files: Vec<String>,
    reference_selected_test_files: Vec<String>,
    context_abstention: Option<ContextAbstention>,
    evaluation_abstained: bool,
    review_abstention: Option<ReviewAbstention>,
    review_changed_files: Vec<String>,
    context_evidence: Vec<String>,
    review_evidence: Vec<String>,
    payload: String,
}

#[derive(Clone, Debug, PartialEq)]
struct CaseScore {
    case_id: &'static str,
    hit_at_5: f64,
    mrr: f64,
    edit_precision: f64,
    edit_recall: f64,
    test_recall: Option<f64>,
    abstention_correct: bool,
    estimated_tokens: usize,
    observation: CaseObservation,
}

#[derive(Clone, Debug, PartialEq)]
struct EvaluationReport {
    case_fingerprint: String,
    fixture_source_fingerprint: String,
    mean_hit_at_5: f64,
    mean_mrr: f64,
    mean_edit_precision: f64,
    mean_edit_recall: f64,
    mean_test_recall: f64,
    abstention_accuracy: f64,
    mean_payload_bytes: f64,
    mean_estimated_tokens: f64,
    scores: Vec<CaseScore>,
}

#[test]
fn locked_v1_1_33_patch_contract_fingerprints_match() {
    assert_eq!(
        fixture::sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(fixture::case_fingerprint(), fixture::V1_CASE_FINGERPRINT);
    assert_eq!(
        fixture::fixture_source_fingerprint(),
        fixture::FIXTURE_SOURCE_FINGERPRINT
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_patch_task_gate_meets_v1_1_33_and_repeats_exactly() {
    let (first, second) = run_live_evaluation()
        .await
        .unwrap_or_else(|error| panic!("native patch-task evaluation failed: {error}"));

    assert_eq!(
        first, second,
        "the complete native report must repeat exactly"
    );
    assert_report_meets_baseline(&first);
    println!(
        "CARTOGRAPH_PATCH_TASK_REPORT=case_fingerprint:{} fixture_fingerprint:{} cases:{} hit_at_5:{:.6} mrr:{:.6} edit_precision:{:.6} edit_recall:{:.6} test_recall:{:.6} abstention:{:.6} mean_payload_bytes:{:.2} mean_estimated_tokens:{:.2}",
        first.case_fingerprint,
        first.fixture_source_fingerprint,
        first.scores.len(),
        first.mean_hit_at_5,
        first.mean_mrr,
        first.mean_edit_precision,
        first.mean_edit_recall,
        first.mean_test_recall,
        first.abstention_accuracy,
        first.mean_payload_bytes,
        first.mean_estimated_tokens,
    );
}

async fn run_live_evaluation() -> EvalResult<(EvaluationReport, EvaluationReport)> {
    let project = tempfile::tempdir().map_err(|_| "patch-task tempdir failed".to_owned())?;
    fixture::materialize(project.path())?;
    initialize_repository(project.path()).await?;
    let runtime = ProjectRuntime::open(project.path())
        .await
        .map_err(|error| format!("patch-task runtime connect failed: {error}"))?;
    let index = runtime
        .index(
            IndexOptions::default()
                .with_max_workers(4)
                .map_err(|error| format!("patch-task worker options failed: {error}"))?,
        )
        .await
        .map_err(|error| format!("patch-task index failed: {error}"))?;
    if index.native.as_ref().map(|metrics| metrics.files) != Some(9) {
        return Err("patch-task index did not publish the nine-file fixture".to_owned());
    }
    let status = runtime
        .status()
        .await
        .map_err(|error| format!("patch-task status failed: {error}"))?;
    if !status.fresh {
        return Err("patch-task fixture was not fresh immediately after indexing".to_owned());
    }

    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let first = evaluate_corpus(&runtime, &retriever, &index.project_id, project.path()).await?;
    let second = evaluate_corpus(&runtime, &retriever, &index.project_id, project.path()).await?;
    runtime
        .close()
        .await
        .map_err(|_| "native evaluation shutdown failed".to_owned())?;
    Ok((first, second))
}

async fn evaluate_corpus(
    runtime: &ProjectRuntime,
    retriever: &DeterministicRetriever,
    project_id: &cartograph_domain::ProjectId,
    project_root: &Path,
) -> EvalResult<EvaluationReport> {
    let mut scores = Vec::with_capacity(fixture::CASES.len());
    for case in fixture::CASES {
        let observation = evaluate_case(runtime, retriever, project_id, project_root, case).await?;
        scores.push(score_case(case, observation));
    }
    let test_scores = scores
        .iter()
        .filter_map(|score| score.test_recall)
        .collect::<Vec<_>>();
    Ok(EvaluationReport {
        case_fingerprint: fixture::case_fingerprint(),
        fixture_source_fingerprint: fixture::fixture_source_fingerprint(),
        mean_hit_at_5: mean(scores.iter().map(|score| score.hit_at_5)),
        mean_mrr: mean(scores.iter().map(|score| score.mrr)),
        mean_edit_precision: mean(scores.iter().map(|score| score.edit_precision)),
        mean_edit_recall: mean(scores.iter().map(|score| score.edit_recall)),
        mean_test_recall: mean(test_scores),
        abstention_accuracy: mean(
            scores
                .iter()
                .map(|score| f64::from(score.abstention_correct)),
        ),
        mean_payload_bytes: mean(
            scores
                .iter()
                .map(|score| usize_as_f64(score.observation.payload.len())),
        ),
        mean_estimated_tokens: mean(
            scores
                .iter()
                .map(|score| usize_as_f64(score.estimated_tokens)),
        ),
        scores,
    })
}

async fn evaluate_case(
    runtime: &ProjectRuntime,
    retriever: &DeterministicRetriever,
    project_id: &cartograph_domain::ProjectId,
    project_root: &Path,
    case: fixture::PatchCase,
) -> EvalResult<CaseObservation> {
    let query = task_query(case.task);
    let budget = ContextBudget::new(cartograph_search::ContextBudgetInput {
        candidate_limit: 8,
        exact_limit: 20,
        traversal: TraversalBudget::new(3, 40)
            .map_err(|error| format!("patch-task traversal budget failed: {error}"))?,
        evidence_limit: 40,
        affected_test_limit: 20,
    })
    .map_err(|error| format!("patch-task context budget failed: {error}"))?;
    let request = ContextRequest::new(
        project_id.clone(),
        query.clone(),
        cartograph_search::ContextRequestOptions::new(IndexFreshness::Current, budget),
    )
    .map_err(|error| format!("patch-task context request failed: {error}"))?;
    let context = retriever
        .context_packet(&request)
        .await
        .map_err(|error| format!("patch-task context retrieval failed: {error}"))?;
    let ranked_symbols = ranked_symbols(runtime, project_id, &context, case.task).await?;
    let evaluation_abstained = ranked_symbols.is_empty();
    let predicted_edit_files = ranked_symbols
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let review = review_prediction(runtime, project_root, &predicted_edit_files).await?;
    let context_selected_test_files = context
        .affected_tests()
        .iter()
        .map(|test| test.symbol().path().as_str().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let review_selected_test_files = review
        .packet()
        .affected_tests()
        .iter()
        .map(|test| test.symbol().path().as_str().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let reference_names = ranked_symbols
        .iter()
        .map(|candidate| candidate.name.clone())
        .chain(
            review
                .packet()
                .evidence()
                .iter()
                .filter(|item| !item.qualified_name().is_empty())
                .map(|item| simple_name(item.qualified_name()).to_owned()),
        )
        .collect::<BTreeSet<_>>();
    let reference_selected_test_files =
        reference_affected_tests(retriever, project_id, &reference_names).await?;
    let selected_test_files = context_selected_test_files
        .iter()
        .chain(&review_selected_test_files)
        .chain(&reference_selected_test_files)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let review_changed_files = review
        .comparison()
        .files()
        .iter()
        .map(|file| file.path().as_str().to_owned())
        .collect::<Vec<_>>();
    let context_evidence = summarize_evidence(context.evidence());
    let review_evidence = summarize_evidence(review.packet().evidence());
    let mut observation = CaseObservation {
        case_id: case.id,
        ranked_symbols,
        predicted_edit_files,
        selected_test_files,
        context_selected_test_files,
        review_selected_test_files,
        reference_selected_test_files,
        context_abstention: context.abstention(),
        evaluation_abstained,
        review_abstention: review.packet().abstention(),
        review_changed_files,
        context_evidence,
        review_evidence,
        payload: String::new(),
    };
    observation.payload = compact_payload(&query, &context, review.packet(), &observation);
    validate_workflow_observation(case, &context, &observation)?;
    Ok(observation)
}

async fn review_prediction(
    runtime: &ProjectRuntime,
    project_root: &Path,
    predicted_edit_files: &[String],
) -> EvalResult<cartograph_agent::ReviewReport> {
    let mut originals = Vec::with_capacity(predicted_edit_files.len());
    for relative in predicted_edit_files {
        let path = project_root.join(relative);
        let original =
            fs::read(&path).map_err(|_| "patch-task predicted file read failed".to_owned())?;
        let mut changed = original.clone();
        changed.extend_from_slice(b"\n// cartograph native patch-task probe\n");
        fs::write(&path, changed).map_err(|_| "patch-task probe write failed".to_owned())?;
        originals.push((path, original));
    }
    let options = ReviewOptions::new("HEAD")
        .map_err(|error| format!("patch-task review options failed: {error}"))?;
    let review = runtime.review(&options).await;
    let mut restore_error = None;
    for (path, original) in originals {
        if fs::write(path, original).is_err() {
            restore_error = Some("patch-task probe restore failed".to_owned());
        }
    }
    if let Some(error) = restore_error {
        return Err(error);
    }
    review.map_err(|error| format!("patch-task review failed: {error}"))
}

async fn ranked_symbols(
    runtime: &ProjectRuntime,
    project_id: &cartograph_domain::ProjectId,
    packet: &ContextPacket,
    task: &str,
) -> EvalResult<Vec<RankedSymbol>> {
    let task_tokens = lexical_tokens(task);
    let primary_edit_paths = packet
        .edit_candidates()
        .candidates()
        .iter()
        .map(|candidate| candidate.path().to_owned())
        .collect::<BTreeSet<_>>();
    let candidate_ids = packet
        .evidence()
        .iter()
        .filter_map(|item| item.bm25_rank().and_then(|_| item.symbol_id()).cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let generation = packet
        .generation()
        .ok_or_else(|| "patch-task packet had no generation".to_owned())?;
    let callable_ids = runtime
        .database()
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            project_id,
            generation.generation_id(),
            &candidate_ids,
        ))
        .await
        .map_err(|error| format!("patch-task symbol hydration failed: {error}"))?
        .into_iter()
        .filter(|symbol| matches!(symbol.symbol_kind(), "function" | "method"))
        .map(|symbol| symbol.symbol_id().clone())
        .collect::<BTreeSet<_>>();
    let mut candidates = packet
        .evidence()
        .iter()
        .filter_map(|item| {
            let rank = item.bm25_rank()?;
            if item
                .symbol_id()
                .is_none_or(|symbol_id| !callable_ids.contains(symbol_id))
                || item.qualified_name().is_empty()
                || cartograph_search::is_test_path(item.path())
                || !primary_edit_paths.contains(item.path())
            {
                return None;
            }
            let name = simple_name(item.qualified_name());
            let mut candidate_tokens = lexical_tokens(name);
            candidate_tokens.extend(lexical_tokens(item.path()));
            if task_tokens.is_disjoint(&candidate_tokens) {
                return None;
            }
            Some(RankedSymbol {
                name: name.to_owned(),
                path: item.path().to_owned(),
                rank,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.name.cmp(&right.name))
    });
    candidates.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    candidates.truncate(TOP_K);
    Ok(candidates)
}

async fn reference_affected_tests(
    retriever: &DeterministicRetriever,
    project_id: &cartograph_domain::ProjectId,
    reference_names: &BTreeSet<String>,
) -> EvalResult<Vec<String>> {
    let mut tests = BTreeSet::new();
    for name in reference_names {
        for reference in retriever
            .exact_reference(
                project_id,
                cartograph_search::ExactTextQuery::new(name, 50)
                    .map_err(|error| format!("patch-task exact-reference input failed: {error}"))?,
            )
            .await
            .map_err(|error| format!("patch-task exact-reference lookup failed: {error}"))?
        {
            if cartograph_search::is_test_path(reference.path().as_str()) {
                tests.insert(reference.path().as_str().to_owned());
            }
        }
    }
    Ok(tests.into_iter().collect())
}

fn simple_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit(['.', ':', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or(qualified_name)
}

fn task_query(task: &str) -> String {
    lexical_tokens(task)
        .into_iter()
        .collect::<Vec<_>>()
        .join(" ")
}

fn lexical_tokens(value: &str) -> BTreeSet<String> {
    const STOP_WORDS: [&str; 22] = [
        "a", "after", "an", "and", "avoid", "before", "change", "creating", "file", "fix", "from",
        "in", "is", "makes", "never", "no", "so", "the", "to", "when", "with", "without",
    ];
    split_words(value)
        .into_iter()
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| token.len() > 1 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn split_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lowercase = false;
    for character in value.chars() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lowercase = false;
            continue;
        }
        if character.is_uppercase() && previous_lowercase && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lowercase = character.is_lowercase();
        current.push(character);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn score_case(case: fixture::PatchCase, observation: CaseObservation) -> CaseScore {
    let expected_symbols = normalized(case.expected_symbols.iter().copied());
    let ranked = observation
        .ranked_symbols
        .iter()
        .take(TOP_K)
        .collect::<Vec<_>>();
    let first_relevant = ranked
        .iter()
        .position(|candidate| expected_symbols.contains(&candidate.name.to_ascii_lowercase()));
    let hit_at_5 = if case.should_abstain {
        f64::from(observation.evaluation_abstained)
    } else {
        f64::from(first_relevant.is_some())
    };
    let mrr = if case.should_abstain {
        f64::from(observation.evaluation_abstained)
    } else {
        first_relevant.map_or(0.0, |index| 1.0 / usize_as_f64(index + 1))
    };
    let edit_precision = precision(&observation.predicted_edit_files, case.expected_edit_files);
    let edit_recall = recall(&observation.predicted_edit_files, case.expected_edit_files);
    let test_recall = (!case.expected_test_files.is_empty())
        .then(|| recall(&observation.selected_test_files, case.expected_test_files));
    let estimated_tokens = observation.payload.len().div_ceil(4);
    CaseScore {
        case_id: case.id,
        hit_at_5,
        mrr,
        edit_precision,
        edit_recall,
        test_recall,
        abstention_correct: observation.evaluation_abstained == case.should_abstain,
        estimated_tokens,
        observation,
    }
}

fn precision(predicted: &[String], expected: &[&str]) -> f64 {
    let predicted = normalized(predicted.iter().map(String::as_str));
    if predicted.is_empty() {
        return f64::from(expected.is_empty());
    }
    let expected = normalized(expected.iter().copied());
    usize_as_f64(predicted.intersection(&expected).count()) / usize_as_f64(predicted.len())
}

fn recall(predicted: &[String], expected: &[&str]) -> f64 {
    let expected = normalized(expected.iter().copied());
    if expected.is_empty() {
        return f64::from(predicted.is_empty());
    }
    let predicted = normalized(predicted.iter().map(String::as_str));
    usize_as_f64(predicted.intersection(&expected).count()) / usize_as_f64(expected.len())
}

fn normalized<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    values.into_iter().map(str::to_ascii_lowercase).collect()
}

fn mean(values: impl IntoIterator<Item = f64>) -> f64 {
    let values = values.into_iter().collect::<Vec<_>>();
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / usize_as_f64(values.len())
    }
}

fn usize_as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn validate_workflow_observation(
    case: fixture::PatchCase,
    context: &ContextPacket,
    observation: &CaseObservation,
) -> EvalResult<()> {
    if observation.review_changed_files != observation.predicted_edit_files {
        return Err(format!(
            "{} review changed-file evidence did not match predicted edits",
            case.id
        ));
    }
    if case.should_abstain {
        validate_abstaining_observation(case, observation)?;
    } else {
        validate_active_observation(case, context, observation)?;
    }
    Ok(())
}

fn validate_abstaining_observation(
    case: fixture::PatchCase,
    observation: &CaseObservation,
) -> EvalResult<()> {
    if !observation.evaluation_abstained {
        return Err(format!("{} failed to abstain", case.id));
    }
    if observation.review_abstention != Some(ReviewAbstention::NoChangedFiles) {
        return Err(format!(
            "{} review did not report no_changed_files",
            case.id
        ));
    }
    Ok(())
}

fn validate_active_observation(
    case: fixture::PatchCase,
    context: &ContextPacket,
    observation: &CaseObservation,
) -> EvalResult<()> {
    if context.abstention().is_some() {
        return Err(format!("{} context unexpectedly abstained", case.id));
    }
    if observation.review_abstention != Some(ReviewAbstention::StaleIndex) {
        return Err(format!(
            "{} dirty review did not report stale_index",
            case.id
        ));
    }
    if observation.review_evidence.is_empty() {
        return Err(format!("{} review returned no graph evidence", case.id));
    }
    if case.id == "watcher-empty-path"
        && recall(
            &observation.review_selected_test_files,
            case.expected_test_files,
        ) < 1.0
    {
        return Err(
            "watcher-empty-path lost its locked reverse-graph affected-test evidence".to_owned(),
        );
    }
    Ok(())
}

fn assert_report_meets_baseline(report: &EvaluationReport) {
    assert_eq!(report.case_fingerprint, fixture::V1_CASE_FINGERPRINT);
    assert_eq!(
        report.fixture_source_fingerprint,
        fixture::FIXTURE_SOURCE_FINGERPRINT
    );
    assert_eq!(report.scores.len(), 5);
    assert!(score_matches(report.mean_hit_at_5, 1.0), "hit@5 regressed");
    assert!(
        score_matches(report.mean_mrr, 1.0),
        "MRR regressed: {:#?}",
        report.scores
    );
    assert!(
        report.mean_edit_precision >= BASELINE_EDIT_PRECISION,
        "edit precision regressed: {}\n{:#?}",
        report.mean_edit_precision,
        report.scores,
    );
    assert!(
        score_matches(report.mean_edit_recall, 1.0),
        "edit recall regressed"
    );
    assert!(
        score_matches(report.mean_test_recall, 1.0),
        "test recall regressed: {:#?}",
        report.scores
    );
    assert!(
        score_matches(report.abstention_accuracy, 1.0),
        "abstention accuracy regressed"
    );
    assert!(
        report.mean_estimated_tokens <= BASELINE_ESTIMATED_TOKENS,
        "payload budget regressed: {} estimated tokens",
        report.mean_estimated_tokens
    );
    for score in &report.scores {
        assert!(
            score_matches(score.hit_at_5, 1.0),
            "{} missed hit@5",
            score.case_id
        );
        assert!(
            score_matches(score.mrr, 1.0),
            "{} MRR regressed",
            score.case_id
        );
        assert!(
            score_matches(score.edit_recall, 1.0),
            "{} edit recall regressed",
            score.case_id
        );
        if let Some(test_recall) = score.test_recall {
            assert!(
                score_matches(test_recall, 1.0),
                "{} affected-test recall regressed",
                score.case_id
            );
        }
        assert!(
            score.abstention_correct,
            "{} abstention regressed",
            score.case_id
        );
    }
}

fn summarize_evidence(evidence: &[EvidenceItem]) -> Vec<String> {
    evidence
        .iter()
        .map(|item| {
            let reasons = item
                .reasons()
                .iter()
                .map(|reason| match reason {
                    EvidenceReason::ExactName => "exact_name",
                    EvidenceReason::ExactPath => "exact_path",
                    EvidenceReason::ExactReference => "exact_reference",
                    EvidenceReason::CoarseReference => "coarse_reference",
                    EvidenceReason::Bm25 => "bm25",
                    EvidenceReason::Semantic => "semantic",
                    EvidenceReason::Graph => "graph",
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{}|{}|{}|{}",
                item.bm25_rank().map_or(0, u16::from),
                item.path(),
                item.qualified_name(),
                reasons
            )
        })
        .collect()
}

fn compact_payload(
    query: &str,
    context: &ContextPacket,
    review: &ReviewPacket,
    observation: &CaseObservation,
) -> String {
    let mut fields = vec![
        format!("case={}", observation.case_id),
        format!("query={query}"),
        format!(
            "context={}|{}|{}|{}",
            freshness_label(context.freshness()),
            confidence_label(context.confidence()),
            context_abstention_label(context.abstention()),
            context.truncated()
        ),
        format!(
            "review={}|{}|{}|{}",
            freshness_label(review.freshness()),
            confidence_label(review.confidence()),
            review_abstention_label(review.abstention()),
            review.truncation().any()
        ),
    ];
    fields.extend(observation.ranked_symbols.iter().map(|candidate| {
        format!(
            "rank={}|{}|{}",
            candidate.rank, candidate.path, candidate.name
        )
    }));
    fields.extend(
        observation
            .predicted_edit_files
            .iter()
            .map(|path| format!("edit={path}")),
    );
    fields.extend(
        observation
            .selected_test_files
            .iter()
            .map(|path| format!("test={path}")),
    );
    fields.extend(
        observation
            .context_selected_test_files
            .iter()
            .map(|path| format!("context_test={path}")),
    );
    fields.extend(
        observation
            .review_selected_test_files
            .iter()
            .map(|path| format!("review_test={path}")),
    );
    fields.extend(
        observation
            .reference_selected_test_files
            .iter()
            .map(|path| format!("reference_test={path}")),
    );
    fields.extend(
        observation
            .context_evidence
            .iter()
            .map(|item| format!("context_evidence={item}")),
    );
    fields.extend(
        observation
            .review_evidence
            .iter()
            .map(|item| format!("review_evidence={item}")),
    );
    fields.join("\n")
}

const fn freshness_label(value: IndexFreshness) -> &'static str {
    match value {
        IndexFreshness::Current => "current",
        IndexFreshness::Stale => "stale",
        IndexFreshness::Unknown => "unknown",
    }
}

const fn confidence_label(value: RetrievalConfidence) -> &'static str {
    match value {
        RetrievalConfidence::None => "none",
        RetrievalConfidence::Low => "low",
        RetrievalConfidence::Medium => "medium",
        RetrievalConfidence::High => "high",
    }
}

const fn context_abstention_label(value: Option<ContextAbstention>) -> &'static str {
    match value {
        None => "none",
        Some(ContextAbstention::NoCurrentGeneration) => "no_current_generation",
        Some(ContextAbstention::NoRelevantEvidence) => "no_relevant_evidence",
        Some(ContextAbstention::StaleIndex) => "stale_index",
        Some(ContextAbstention::UnknownFreshness) => "unknown_freshness",
    }
}

const fn review_abstention_label(value: Option<ReviewAbstention>) -> &'static str {
    match value {
        None => "none",
        Some(ReviewAbstention::NoCurrentGeneration) => "no_current_generation",
        Some(ReviewAbstention::NoChangedFiles) => "no_changed_files",
        Some(ReviewAbstention::NoIndexedChangedFiles) => "no_indexed_changed_files",
        Some(ReviewAbstention::StaleIndex) => "stale_index",
        Some(ReviewAbstention::UnknownFreshness) => "unknown_freshness",
    }
}

async fn initialize_repository(project_root: &Path) -> EvalResult<()> {
    for arguments in [
        &["init", "--initial-branch=main"][..],
        &["config", "user.email", "patch-eval@example.invalid"][..],
        &["config", "user.name", "Patch Evaluation"][..],
        &["add", "."][..],
        &["commit", "-m", "locked v1.1.33 patch fixture"][..],
    ] {
        run_fixture_git(project_root, arguments).await?;
    }
    Ok(())
}

async fn run_fixture_git(project_root: &Path, arguments: &[&str]) -> EvalResult<()> {
    let hooks_path = project_root.join(".git/disabled-hooks");
    let global_config = project_root.join(".git/disabled-global-config");
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor=false"])
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks_path.display()))
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "tag.gpgSign=false"])
        .arg("-C")
        .arg(project_root)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_DIR")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "patch-task Git fixture command failed to start".to_owned())?;
    match tokio::time::timeout(GIT_COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => Err("patch-task Git fixture command failed".to_owned()),
        Ok(Err(_)) => {
            terminate_child(&mut child).await;
            Err("patch-task Git fixture command wait failed".to_owned())
        }
        Err(_) => {
            terminate_child(&mut child).await;
            Err("patch-task Git fixture command exceeded its deadline".to_owned())
        }
    }
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}
