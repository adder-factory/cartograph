use crate::PipelineStage;
use cartograph_domain::NormalizedPath;
use std::fmt;
use thiserror::Error;

/// Credential-safe failure returned by a supervised pipeline future.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("Cartograph indexing failed during the {stage} stage")]
pub struct PipelineFailure {
    stage: PipelineStage,
    detail: PipelineFailureDetail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PipelineFailureDetail {
    None,
    Reason(PipelineFailureReason),
    File(PipelineFileFailure),
}

/// One bounded project-relative input and its allowlisted failure reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineFileFailure {
    path: NormalizedPath,
    reason: PipelineFailureReason,
}

impl PipelineFileFailure {
    /// Bind one allowlisted failure reason to a validated project-relative path.
    #[must_use]
    pub const fn new(path: NormalizedPath, reason: PipelineFailureReason) -> Self {
        Self { path, reason }
    }

    /// Exact normalized path relative to the configured project root.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Stable credential-safe reason for this input failure.
    #[must_use]
    pub const fn reason(&self) -> PipelineFailureReason {
        self.reason
    }
}

impl fmt::Display for PipelineFileFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug-format only the already-normalized relative string so control
        // characters cannot inject another terminal/log line.
        write!(
            formatter,
            "{} at project-relative path {:?}",
            self.reason,
            self.path.as_str()
        )
    }
}

/// Stable credential-safe detail for a pipeline failure whose cause is actionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineFailureReason {
    /// A bounded item or whole-stage execution horizon elapsed, or one of the
    /// stage's database statements outlived its statement timeout.
    DeadlineExceeded,
    /// The supervisor observed no durable stage progress inside its watchdog horizon.
    ProgressStalled,
    /// The configured in-memory generation ceiling rejected bounded work.
    GenerationCapacityExceeded,
    /// One manifest input could not be reopened under the bounded source contract.
    SourceReadFailed,
    /// One source changed after its exact manifest entry was read.
    SourceChangedDuringParse,
    /// Discovery admitted a language without a production extractor.
    ExtractionUnsupportedLanguage,
    /// A reusable extractor received a snapshot for another language.
    ExtractionLanguageMismatch,
    /// A statically linked grammar was unavailable or ABI-incompatible.
    ExtractionGrammarUnavailable,
    /// The parser stopped before returning a syntax tree.
    ExtractionParserStopped,
    /// Cooperative cancellation interrupted one extraction input.
    ExtractionCancelled,
    /// The parser returned a source span outside the durable contract.
    ExtractionInvalidSpan,
    /// The configured AST-depth policy was outside the supported range.
    ExtractionInvalidNestingLimit,
    /// One source syntax tree exceeded the extractor's defensive nesting ceiling.
    ExtractionNestingLimitExceeded,
    /// One source file exceeded its bounded modeled extraction output allowance.
    ExtractionOutputLimitExceeded,
    /// One file-local parse operation failed without a more specific safe classification.
    FileProcessingFailed,
    /// Canonical reduction rejected an extracted reference name above the storage bound.
    ReferenceNameTooLong,
    /// Canonical reduction rejected one bounded field, named by its stable
    /// storage field identifier so the failure is actionable without bisecting
    /// the corpus by hand.
    CanonicalFieldRejected(&'static str),
}

#[derive(Clone, Copy)]
struct PipelineFailureText {
    code: &'static str,
    description: &'static str,
}

impl PipelineFailureText {
    const fn new(code: &'static str, description: &'static str) -> Self {
        Self { code, description }
    }
}

const FILE_PROCESSING_FAILURE_TEXT: PipelineFailureText = PipelineFailureText::new(
    "file_processing_failed",
    "file-local parse processing failed without a more specific safe classification",
);

const fn pipeline_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::DeadlineExceeded
        | PipelineFailureReason::ProgressStalled
        | PipelineFailureReason::GenerationCapacityExceeded
        | PipelineFailureReason::SourceReadFailed
        | PipelineFailureReason::SourceChangedDuringParse => pipeline_control_failure_text(reason),
        PipelineFailureReason::ExtractionUnsupportedLanguage
        | PipelineFailureReason::ExtractionLanguageMismatch
        | PipelineFailureReason::ExtractionGrammarUnavailable
        | PipelineFailureReason::ExtractionParserStopped
        | PipelineFailureReason::ExtractionCancelled
        | PipelineFailureReason::ExtractionInvalidSpan
        | PipelineFailureReason::ExtractionInvalidNestingLimit
        | PipelineFailureReason::ExtractionNestingLimitExceeded
        | PipelineFailureReason::ExtractionOutputLimitExceeded
        | PipelineFailureReason::FileProcessingFailed => pipeline_extraction_failure_text(reason),
        PipelineFailureReason::ReferenceNameTooLong
        | PipelineFailureReason::CanonicalFieldRejected(_) => {
            pipeline_reduction_failure_text(reason)
        }
    }
}

const fn pipeline_control_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::DeadlineExceeded => PipelineFailureText::new(
            "deadline_exceeded",
            "the bounded item or stage deadline elapsed",
        ),
        PipelineFailureReason::ProgressStalled => PipelineFailureText::new(
            "progress_stalled",
            "the stage made no durable progress inside its watchdog horizon",
        ),
        PipelineFailureReason::GenerationCapacityExceeded => PipelineFailureText::new(
            "generation_capacity_exceeded",
            "the configured generation capacity rejected this work",
        ),
        PipelineFailureReason::SourceReadFailed => PipelineFailureText::new(
            "source_read_failed",
            "the source file could not be reopened under the bounded source contract",
        ),
        PipelineFailureReason::SourceChangedDuringParse => PipelineFailureText::new(
            "source_changed_during_parse",
            "the source file changed after its manifest entry was read",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

const fn pipeline_extraction_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::ExtractionUnsupportedLanguage => PipelineFailureText::new(
            "extraction_unsupported_language",
            "no production extractor was available for the admitted source language",
        ),
        PipelineFailureReason::ExtractionLanguageMismatch => PipelineFailureText::new(
            "extraction_language_mismatch",
            "the selected extractor did not match the source language",
        ),
        PipelineFailureReason::ExtractionGrammarUnavailable => PipelineFailureText::new(
            "extraction_grammar_unavailable",
            "the statically linked parser grammar was unavailable",
        ),
        PipelineFailureReason::ExtractionParserStopped => PipelineFailureText::new(
            "extraction_parser_stopped",
            "the parser stopped before producing a syntax tree",
        ),
        PipelineFailureReason::ExtractionCancelled => PipelineFailureText::new(
            "extraction_cancelled",
            "cooperative cancellation interrupted source extraction",
        ),
        PipelineFailureReason::ExtractionInvalidSpan => PipelineFailureText::new(
            "extraction_invalid_span",
            "the parser produced a source span outside the durable contract",
        ),
        PipelineFailureReason::ExtractionInvalidNestingLimit => PipelineFailureText::new(
            "extraction_invalid_nesting_limit",
            "the AST-depth policy was outside the supported range",
        ),
        PipelineFailureReason::ExtractionNestingLimitExceeded => PipelineFailureText::new(
            "extraction_nesting_limit_exceeded",
            "the source syntax tree exceeded the configured nesting limit",
        ),
        PipelineFailureReason::ExtractionOutputLimitExceeded => PipelineFailureText::new(
            "extraction_output_limit_exceeded",
            "the extracted file facts exceeded the configured output limit",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

const fn pipeline_reduction_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::ReferenceNameTooLong => PipelineFailureText::new(
            "reference_name_too_long",
            "an extracted reference name exceeded the canonical storage bound",
        ),
        PipelineFailureReason::CanonicalFieldRejected(_) => PipelineFailureText::new(
            "canonical_field_rejected",
            "an extracted field was rejected by the canonical storage contract",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

impl PipelineFailureReason {
    /// Stable machine-readable reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        pipeline_failure_text(self).code
    }

    /// Fixed user-facing explanation that never contains source, paths, or driver text.
    #[must_use]
    pub const fn description(self) -> &'static str {
        pipeline_failure_text(self).description
    }

    /// Stable storage field identifier for a rejected bounded field.
    #[must_use]
    pub const fn rejected_field(self) -> Option<&'static str> {
        match self {
            Self::CanonicalFieldRejected(field) => Some(field),
            _ => None,
        }
    }
}

impl fmt::Display for PipelineFailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())?;
        // Naming the exact rejected field turns a whole-corpus bisection into a
        // single run. The identifier is a fixed storage field name, never source
        // text or a checkout path.
        match self.rejected_field() {
            Some(field) => write!(formatter, "({field})"),
            None => Ok(()),
        }
    }
}

impl PipelineFailure {
    /// Identify the stable stage without accepting arbitrary error text.
    #[must_use]
    pub const fn new(stage: PipelineStage) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::None,
        }
    }

    /// Attach one allowlisted actionable reason without retaining source or driver text.
    #[must_use]
    pub const fn with_reason(stage: PipelineStage, reason: PipelineFailureReason) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::Reason(reason),
        }
    }

    /// Attach one validated project-relative input and allowlisted reason.
    #[must_use]
    pub const fn with_file_failure(stage: PipelineStage, failure: PipelineFileFailure) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::File(failure),
        }
    }

    /// Stable failed stage recorded by the supervisor and structured workers.
    #[must_use]
    pub const fn stage(&self) -> PipelineStage {
        self.stage
    }

    /// Optional allowlisted actionable reason.
    #[must_use]
    pub const fn reason(&self) -> Option<PipelineFailureReason> {
        match &self.detail {
            PipelineFailureDetail::None => None,
            PipelineFailureDetail::Reason(reason) => Some(*reason),
            PipelineFailureDetail::File(failure) => Some(failure.reason()),
        }
    }

    /// Optional exact project-relative input that caused the stage failure.
    #[must_use]
    pub const fn file_failure(&self) -> Option<&PipelineFileFailure> {
        match &self.detail {
            PipelineFailureDetail::File(failure) => Some(failure),
            PipelineFailureDetail::None | PipelineFailureDetail::Reason(_) => None,
        }
    }
}
