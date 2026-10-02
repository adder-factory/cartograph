use cartograph_agent::{IndexFailure, PipelineFailureReason, PipelineStage, ProjectError};
use serde::Serialize;

pub(crate) const GENERATION_CAPACITY_LIMIT: &str = "maxGenerationBytes";
pub(crate) const GENERATION_CAPACITY_SCOPE: &str = "cartograph_process";
pub(crate) const GENERATION_CAPACITY_NEXT_ACTION: &str = "use generationStorage=postgres for a dense repository; maxGenerationBytes cannot exceed 8589934592 bytes (8 GiB), so when that ceiling is already selected exclude machine-generated or compiled artifacts with --exclude or project configuration, then run an explicit cartograph index; auto-sync suppresses itself after five capacity failures";
/// Stable code for a failed bounded cleanup of index staging state.
pub(crate) const INDEX_CLEANUP_FAILED_CODE: &str = "index_cleanup_failed";

pub(crate) const fn is_generation_capacity_failure(error: &ProjectError) -> bool {
    matches!(
        error,
        ProjectError::IndexStageFailedWithReason {
            reason: PipelineFailureReason::GenerationCapacityExceeded,
            ..
        }
    )
}

/// Stable machine-readable codes one pipeline stage can report.
struct StageCodes {
    failed: &'static str,
    deadline_exceeded: &'static str,
    progress_stalled: &'static str,
}

/// Every stage's code family in one place, so a new stage cannot be added to
/// one lookup and forgotten in another.
const STAGE_CODES: [(PipelineStage, StageCodes); 11] = [
    (
        PipelineStage::Discover,
        StageCodes {
            failed: "discover_failed",
            deadline_exceeded: "discover_deadline_exceeded",
            progress_stalled: "discover_progress_stalled",
        },
    ),
    (
        PipelineStage::Read,
        StageCodes {
            failed: "read_failed",
            deadline_exceeded: "read_deadline_exceeded",
            progress_stalled: "read_progress_stalled",
        },
    ),
    (
        PipelineStage::Parse,
        StageCodes {
            failed: "parse_failed",
            deadline_exceeded: "parse_deadline_exceeded",
            progress_stalled: "parse_progress_stalled",
        },
    ),
    (
        PipelineStage::Resolve,
        StageCodes {
            failed: "resolve_failed",
            deadline_exceeded: "resolve_deadline_exceeded",
            progress_stalled: "resolve_progress_stalled",
        },
    ),
    (
        PipelineStage::Overlay,
        StageCodes {
            failed: "overlay_failed",
            deadline_exceeded: "overlay_deadline_exceeded",
            progress_stalled: "overlay_progress_stalled",
        },
    ),
    (
        PipelineStage::Reduce,
        StageCodes {
            failed: "reduce_failed",
            deadline_exceeded: "reduce_deadline_exceeded",
            progress_stalled: "reduce_progress_stalled",
        },
    ),
    (
        PipelineStage::Copy,
        StageCodes {
            failed: "copy_failed",
            deadline_exceeded: "copy_deadline_exceeded",
            progress_stalled: "copy_progress_stalled",
        },
    ),
    (
        PipelineStage::RelationalMerge,
        StageCodes {
            failed: "relational_merge_failed",
            deadline_exceeded: "relational_merge_deadline_exceeded",
            progress_stalled: "relational_merge_progress_stalled",
        },
    ),
    (
        PipelineStage::Bm25,
        StageCodes {
            failed: "bm25_failed",
            deadline_exceeded: "bm25_deadline_exceeded",
            progress_stalled: "bm25_progress_stalled",
        },
    ),
    (
        PipelineStage::Vector,
        StageCodes {
            failed: "vector_failed",
            deadline_exceeded: "vector_deadline_exceeded",
            progress_stalled: "vector_progress_stalled",
        },
    ),
    (
        PipelineStage::Publish,
        StageCodes {
            failed: "publication_failed",
            deadline_exceeded: "publication_deadline_exceeded",
            progress_stalled: "publication_progress_stalled",
        },
    ),
];

const fn stage_codes(stage: PipelineStage) -> &'static StageCodes {
    let mut index = 0;
    while index < STAGE_CODES.len() {
        let (candidate, ref codes) = STAGE_CODES[index];
        if candidate as u8 == stage as u8 {
            return codes;
        }
        index += 1;
    }
    // Every stage is listed above; the table is exhaustive by construction.
    &STAGE_CODES[0].1
}

const fn pipeline_stage_failure_code(stage: PipelineStage) -> &'static str {
    stage_codes(stage).failed
}

const fn pipeline_failure_reason_code(
    stage: PipelineStage,
    reason: PipelineFailureReason,
) -> &'static str {
    match reason {
        PipelineFailureReason::DeadlineExceeded => pipeline_deadline_code(stage),
        PipelineFailureReason::ProgressStalled => pipeline_progress_stalled_code(stage),
        _ => pipeline_stage_reason_code(stage, reason),
    }
}

const fn pipeline_stage_reason_code(
    stage: PipelineStage,
    reason: PipelineFailureReason,
) -> &'static str {
    match stage {
        PipelineStage::Parse => parse_failure_reason_code(reason),
        PipelineStage::Resolve => resolve_failure_reason_code(reason),
        PipelineStage::Reduce => reduce_failure_reason_code(reason),
        _ => pipeline_stage_failure_code(stage),
    }
}

const fn parse_failure_reason_code(reason: PipelineFailureReason) -> &'static str {
    match reason {
        PipelineFailureReason::GenerationCapacityExceeded => "parse_generation_capacity_exceeded",
        PipelineFailureReason::SourceReadFailed
        | PipelineFailureReason::SourceChangedDuringParse => parse_source_failure_code(reason),
        PipelineFailureReason::ExtractionUnsupportedLanguage
        | PipelineFailureReason::ExtractionLanguageMismatch
        | PipelineFailureReason::ExtractionGrammarUnavailable
        | PipelineFailureReason::ExtractionParserStopped
        | PipelineFailureReason::ExtractionCancelled
        | PipelineFailureReason::ExtractionInvalidSpan
        | PipelineFailureReason::ExtractionInvalidNestingLimit
        | PipelineFailureReason::ExtractionNestingLimitExceeded
        | PipelineFailureReason::ExtractionOutputLimitExceeded
        | PipelineFailureReason::FileProcessingFailed => parse_extraction_failure_code(reason),
        _ => pipeline_stage_failure_code(PipelineStage::Parse),
    }
}

const fn parse_source_failure_code(reason: PipelineFailureReason) -> &'static str {
    match reason {
        PipelineFailureReason::SourceReadFailed => "parse_source_read_failed",
        PipelineFailureReason::SourceChangedDuringParse => "parse_source_changed",
        _ => pipeline_stage_failure_code(PipelineStage::Parse),
    }
}

const fn parse_extraction_failure_code(reason: PipelineFailureReason) -> &'static str {
    match reason {
        PipelineFailureReason::ExtractionNestingLimitExceeded => {
            "parse_extraction_nesting_limit_exceeded"
        }
        PipelineFailureReason::ExtractionOutputLimitExceeded => {
            "parse_extraction_output_limit_exceeded"
        }
        PipelineFailureReason::ExtractionUnsupportedLanguage => "parse_unsupported_language",
        PipelineFailureReason::ExtractionLanguageMismatch => "parse_language_mismatch",
        PipelineFailureReason::ExtractionGrammarUnavailable => "parse_grammar_unavailable",
        PipelineFailureReason::ExtractionParserStopped => "parse_parser_stopped",
        PipelineFailureReason::ExtractionCancelled => "parse_extraction_cancelled",
        PipelineFailureReason::ExtractionInvalidSpan => "parse_invalid_span",
        PipelineFailureReason::ExtractionInvalidNestingLimit => "parse_invalid_nesting_limit",
        PipelineFailureReason::FileProcessingFailed => "parse_file_processing_failed",
        _ => pipeline_stage_failure_code(PipelineStage::Parse),
    }
}

const fn resolve_failure_reason_code(reason: PipelineFailureReason) -> &'static str {
    match reason {
        PipelineFailureReason::GenerationCapacityExceeded => "resolve_generation_capacity_exceeded",
        _ => pipeline_stage_failure_code(PipelineStage::Resolve),
    }
}

const fn reduce_failure_reason_code(reason: PipelineFailureReason) -> &'static str {
    match reason {
        PipelineFailureReason::GenerationCapacityExceeded => "reduce_generation_capacity_exceeded",
        PipelineFailureReason::ReferenceNameTooLong => "reduce_reference_name_too_long",
        PipelineFailureReason::CanonicalFieldRejected(_) => "reduce_canonical_field_rejected",
        _ => pipeline_stage_failure_code(PipelineStage::Reduce),
    }
}

/// Stable code for a failure of the index pipeline itself.
///
/// Callers layer their own surface-specific arms on top; the shared indexing
/// vocabulary lives here so the CLI and the MCP admin surface cannot drift.
pub(crate) const fn index_stage_failure_code(error: &ProjectError) -> Option<&'static str> {
    match error {
        ProjectError::IndexStageFailed { stage } => Some(pipeline_stage_failure_code(*stage)),
        ProjectError::IndexStageFailedWithReason { stage, reason } => {
            Some(pipeline_failure_reason_code(*stage, *reason))
        }
        ProjectError::IndexStageFileFailed { stage, failure } => {
            Some(pipeline_failure_reason_code(*stage, failure.reason()))
        }
        other => index_lifecycle_failure_code(other),
    }
}

/// Failures outside supervised stages: discovery, ownership, publication, and setup.
const fn index_lifecycle_failure_code(error: &ProjectError) -> Option<&'static str> {
    match error {
        ProjectError::BeginGenerationFailed => Some("generation_start_failed"),
        ProjectError::SourceScanFailed => Some("source_scan_failed"),
        ProjectError::SourceChangedDuringIndex => Some("source_changed_during_index"),
        ProjectError::IndexFailed => Some("index_failed"),
        ProjectError::IndexLeaseBusy => Some("lease_busy"),
        ProjectError::IndexRetentionBacklog => Some("retention_backlog"),
        ProjectError::IndexLeaseFailed => Some("lease_failed"),
        ProjectError::IndexPublicationFailed => Some("publication_failed"),
        ProjectError::IndexCleanupFailed => Some(INDEX_CLEANUP_FAILED_CODE),
        ProjectError::ScipOverlayInvalid => Some("scip_overlay_invalid"),
        ProjectError::ProjectConfiguration(_) => Some("project_configuration_invalid"),
        _ => None,
    }
}

pub(crate) const fn project_index_failure_code(error: &ProjectError) -> Option<&'static str> {
    match error {
        ProjectError::RequestCancelled => Some("request_cancelled"),
        other => index_stage_failure_code(other),
    }
}

/// Text for one failed direct index attempt: the primary failure, then any
/// secondary cleanup failure so it cannot be mistaken for the primary code.
pub(crate) fn direct_index_attempt_failure_message(failure: &IndexFailure) -> String {
    let primary = direct_index_failure_message(failure.error());
    if failure.cleanup_failed() {
        let cleanup = ProjectError::IndexCleanupFailed;
        format!("{primary}; secondary failure: {cleanup} (reason: {INDEX_CLEANUP_FAILED_CODE})")
    } else {
        primary
    }
}

pub(crate) fn direct_index_failure_message(error: &ProjectError) -> String {
    let message = project_index_failure_code(error).map_or_else(
        || error.to_string(),
        |code| format!("{error} (reason: {code})"),
    );
    if is_generation_capacity_failure(error) {
        format!(
            "{message}; limit: {GENERATION_CAPACITY_LIMIT}; scope: {GENERATION_CAPACITY_SCOPE}; next action: {GENERATION_CAPACITY_NEXT_ACTION}"
        )
    } else {
        message
    }
}

#[derive(Serialize)]
struct DirectIndexFailureReport<'failure> {
    error: DirectIndexFailure<'failure>,
}

#[derive(Serialize)]
struct DirectIndexFailure<'failure> {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<PipelineStage>,
    /// Observed after the failure; `null` when that lookup itself failed.
    previous_generation_visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<SecondaryFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity_limit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_failure: Option<DirectFileFailure<'failure>>,
}

/// A failure observed after, and subordinate to, the primary failure.
///
/// Direct `index --format json` reports it as `cleanup_failure` and MCP admin
/// job status as `cleanupFailure`; both carry the same `code` and `message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SecondaryFailure {
    code: &'static str,
    message: String,
}

impl SecondaryFailure {
    /// The secondary cleanup failure `failure` carries, if any: bounded
    /// cleanup of the attempt's own staging generation also failed, and the
    /// next index attempt retries it.
    pub(crate) fn index_cleanup(failure: &IndexFailure) -> Option<Self> {
        failure.cleanup_failed().then(|| Self {
            code: INDEX_CLEANUP_FAILED_CODE,
            message: ProjectError::IndexCleanupFailed.to_string(),
        })
    }
}

/// One failed direct index attempt plus what readers still see afterward.
#[derive(Clone, Copy)]
pub(crate) struct DirectIndexFailureInput<'failure> {
    /// The primary failure and any secondary cleanup failure.
    pub(crate) failure: &'failure IndexFailure,
    /// Whether a published generation is still current; `None` when unknown.
    pub(crate) previous_generation_visible: Option<bool>,
}

#[derive(Serialize)]
struct DirectFileFailure<'failure> {
    path: &'failure str,
    reason: &'static str,
    description: &'static str,
}

/// Serialize one direct-index failure without adding the ordinary text prefix.
///
/// `code` is always the primary failure; a cleanup that also failed is the
/// separate `cleanup_failure` object, and `previous_generation_visible` is the
/// observed database state rather than an assumption about the error kind.
pub(crate) fn direct_index_failure_json(
    input: DirectIndexFailureInput<'_>,
) -> Result<String, serde_json::Error> {
    let error = input.failure.error();
    let (stage, file_failure) = match error {
        ProjectError::IndexStageFailed { stage }
        | ProjectError::IndexStageFailedWithReason { stage, .. } => (Some(*stage), None),
        ProjectError::IndexStageFileFailed { stage, failure } => (
            Some(*stage),
            Some(DirectFileFailure {
                path: failure.path().as_str(),
                reason: failure.reason().as_str(),
                description: failure.reason().description(),
            }),
        ),
        _ => (None, None),
    };
    let capacity = is_generation_capacity_failure(error);
    serde_json::to_string_pretty(&DirectIndexFailureReport {
        error: DirectIndexFailure {
            code: project_index_failure_code(error).unwrap_or("index_failed"),
            message: error.to_string(),
            stage,
            previous_generation_visible: input.previous_generation_visible,
            cleanup_failure: SecondaryFailure::index_cleanup(input.failure),
            capacity_limit: capacity.then_some(GENERATION_CAPACITY_LIMIT),
            capacity_scope: capacity.then_some(GENERATION_CAPACITY_SCOPE),
            next_action: capacity.then_some(GENERATION_CAPACITY_NEXT_ACTION),
            file_failure,
        },
    })
}

const fn pipeline_deadline_code(stage: PipelineStage) -> &'static str {
    stage_codes(stage).deadline_exceeded
}

const fn pipeline_progress_stalled_code(stage: PipelineStage) -> &'static str {
    stage_codes(stage).progress_stalled
}

#[cfg(test)]
mod tests {
    use super::*;
    use cartograph_agent::PipelineFileFailure;
    use cartograph_config::ProjectConfigError;
    use cartograph_domain::NormalizedPath;

    #[test]
    fn direct_cli_progress_stall_message_includes_the_qualified_stable_code() {
        let error = ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Resolve,
            reason: PipelineFailureReason::ProgressStalled,
        };
        let message = direct_index_failure_message(&error);
        assert!(message.contains("resolve/progress_stalled"));
        assert!(message.ends_with("(reason: resolve_progress_stalled)"));
    }

    #[test]
    fn direct_cli_capacity_failure_names_the_bound_scope_and_next_action() {
        let error = ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Resolve,
            reason: PipelineFailureReason::GenerationCapacityExceeded,
        };
        let message = direct_index_failure_message(&error);
        assert!(message.contains("resolve_generation_capacity_exceeded"));
        assert!(message.contains(GENERATION_CAPACITY_LIMIT));
        assert!(message.contains(GENERATION_CAPACITY_SCOPE));
        assert!(message.contains("generationStorage=postgres"));
        assert!(message.contains("8589934592 bytes (8 GiB)"));
        assert!(message.contains("exclude machine-generated or compiled artifacts"));
        assert!(!message.contains("raise maxGenerationBytes"));

        let encoded = direct_index_failure_json(DirectIndexFailureInput {
            failure: &IndexFailure::from(error),
            previous_generation_visible: Some(true),
        })
        .unwrap_or_else(|serialization| {
            panic!("capacity failure JSON was unavailable: {serialization}")
        });
        let report: serde_json::Value = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("capacity failure JSON was invalid: {error}"));
        assert_eq!(
            report["error"]["code"],
            "resolve_generation_capacity_exceeded"
        );
        assert_eq!(report["error"]["capacity_limit"], GENERATION_CAPACITY_LIMIT);
        assert_eq!(report["error"]["capacity_scope"], GENERATION_CAPACITY_SCOPE);
        assert!(
            report["error"]["next_action"]
                .as_str()
                .is_some_and(|action| action.contains("maxGenerationBytes"))
        );
    }

    #[test]
    fn direct_cli_configuration_failure_names_the_field_range_and_stable_code() {
        let error =
            ProjectError::ProjectConfiguration(ProjectConfigError::NumericFieldOutOfRange {
                field: "maxGenerationBytes",
                minimum: 1,
                maximum: 8_589_934_592,
            });
        let message = direct_index_failure_message(&error);
        assert!(message.contains("`maxGenerationBytes`"));
        assert!(message.contains("8589934592"));
        let encoded = direct_index_failure_json(DirectIndexFailureInput {
            failure: &IndexFailure::from(error),
            previous_generation_visible: Some(false),
        })
        .unwrap_or_else(|serialization| panic!("config JSON failed: {serialization}"));
        let report: serde_json::Value = serde_json::from_str(&encoded)
            .unwrap_or_else(|serialization| panic!("config JSON was invalid: {serialization}"));
        assert_eq!(report["error"]["code"], "project_configuration_invalid");
        assert!(
            report["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("maxGenerationBytes"))
        );
    }

    fn failure_report(input: DirectIndexFailureInput<'_>) -> serde_json::Value {
        let encoded = direct_index_failure_json(input)
            .unwrap_or_else(|serialization| panic!("failure JSON failed: {serialization}"));
        serde_json::from_str(&encoded)
            .unwrap_or_else(|serialization| panic!("failure JSON was invalid: {serialization}"))
    }

    #[test]
    fn direct_cli_cleanup_failure_is_secondary_to_the_retryable_primary_code() {
        let failure = IndexFailure::from(ProjectError::IndexLeaseBusy).with_failed_cleanup();
        let report = failure_report(DirectIndexFailureInput {
            failure: &failure,
            previous_generation_visible: Some(true),
        });
        assert_eq!(report["error"]["code"], "lease_busy");
        assert_eq!(report["error"]["previous_generation_visible"], true);
        assert_eq!(
            report["error"]["cleanup_failure"]["code"],
            "index_cleanup_failed"
        );
        assert!(
            report["error"]["cleanup_failure"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("cleanup failed"))
        );

        let message = direct_index_attempt_failure_message(&failure);
        assert!(message.starts_with(&ProjectError::IndexLeaseBusy.to_string()));
        assert!(message.contains("(reason: lease_busy); secondary failure:"));
        assert!(message.ends_with("(reason: index_cleanup_failed)"));
    }

    #[test]
    fn direct_cli_visibility_reports_observed_state_not_the_error_kind() {
        let cleanup = IndexFailure::from(ProjectError::IndexCleanupFailed);
        let visible = failure_report(DirectIndexFailureInput {
            failure: &cleanup,
            previous_generation_visible: Some(true),
        });
        assert_eq!(visible["error"]["code"], "index_cleanup_failed");
        assert_eq!(visible["error"]["previous_generation_visible"], true);
        assert!(visible["error"].get("cleanup_failure").is_none());

        let first_index = IndexFailure::from(ProjectError::IndexLeaseBusy);
        let absent = failure_report(DirectIndexFailureInput {
            failure: &first_index,
            previous_generation_visible: Some(false),
        });
        assert_eq!(absent["error"]["previous_generation_visible"], false);

        let unknown = failure_report(DirectIndexFailureInput {
            failure: &first_index,
            previous_generation_visible: None,
        });
        assert!(unknown["error"]["previous_generation_visible"].is_null());
        assert_eq!(
            direct_index_attempt_failure_message(&first_index),
            direct_index_failure_message(&ProjectError::IndexLeaseBusy)
        );
    }

    #[test]
    fn direct_cli_file_failure_is_structured_relative_and_terminal_safe() {
        let path = NormalizedPath::parse("src/broken\nfile.rs")
            .unwrap_or_else(|error| panic!("fixture path was invalid: {error}"));
        let error = ProjectError::IndexStageFileFailed {
            stage: PipelineStage::Parse,
            failure: PipelineFileFailure::new(
                path,
                PipelineFailureReason::SourceChangedDuringParse,
            ),
        };

        let message = direct_index_failure_message(&error);
        assert_eq!(message.lines().count(), 1);
        assert!(message.contains("src/broken\\nfile.rs"));
        assert!(message.ends_with("(reason: parse_source_changed)"));

        let encoded = direct_index_failure_json(DirectIndexFailureInput {
            failure: &IndexFailure::from(error),
            previous_generation_visible: Some(true),
        })
        .unwrap_or_else(|serialization| panic!("failure JSON was unavailable: {serialization}"));
        let report: serde_json::Value = serde_json::from_str(&encoded)
            .unwrap_or_else(|serialization| panic!("failure JSON was invalid: {serialization}"));
        assert_eq!(report["error"]["code"], "parse_source_changed");
        assert_eq!(report["error"]["stage"], "parse");
        assert_eq!(report["error"]["previous_generation_visible"], true);
        assert_eq!(
            report["error"]["file_failure"]["path"],
            "src/broken\nfile.rs"
        );
        assert_eq!(
            report["error"]["file_failure"]["reason"],
            "source_changed_during_parse"
        );
        assert!(
            report["error"]["file_failure"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("manifest entry"))
        );
    }
}
