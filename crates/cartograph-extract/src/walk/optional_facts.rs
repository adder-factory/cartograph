//! Optional enrichment facts and the fallback pass that omits them.
//!
//! Some facts are enrichment beyond a file's core declarations and calls:
//! per-name package bindings, struct fields, constant reads, literal
//! instantiations, declared-type uses, supertraits, and decorators. In dense
//! generated code (`iota` tables, opcode lists, constant-read tables) they can
//! multiply the retained output of a file that otherwise extracts well within
//! its per-file ceiling. A file must never become unextractable, and so fail
//! its whole generation, only because of those facts: when a pass that
//! recorded any of them exceeds the output limit, in the walk or in a later
//! stage that shares the file's output limit (framework and test-name
//! enrichment), the file is extracted again without them and carries an
//! [`DiagnosticCode::OptionalFactsOmitted`] diagnostic instead. The fallback
//! pass records no fact beyond those of the extractor before optional facts
//! existed (it keeps only refinements that cost no output, such as the
//! static-method flag and `implements` in place of `extends`), so it fits
//! wherever that extractor fit. The diagnostic is added only when it fits
//! too, so it can never be what fails the file.
//!
//! [`DiagnosticCode::OptionalFactsOmitted`]: crate::DiagnosticCode::OptionalFactsOmitted

use crate::{
    DiagnosticCode, ExtractError, ExtractedFile, ExtractionDiagnostic, SourceSnapshot,
    budget::native_output_limit,
};

/// Whether an extraction pass records optional enrichment facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OptionalFacts {
    /// The first pass: optional facts are recorded.
    Recorded,
    /// The fallback pass after an output-limit failure: optional facts are
    /// omitted and the file says so with a diagnostic.
    Omitted,
}

/// A pass's optional-fact policy and whether any optional extractor ran.
#[derive(Clone, Copy, Debug)]
pub(super) struct OptionalFactGate {
    policy: OptionalFacts,
    used: bool,
}

impl OptionalFactGate {
    pub(super) const fn new(policy: OptionalFacts) -> Self {
        Self {
            policy,
            used: false,
        }
    }

    /// Whether an optional extractor may record its facts in this pass. An
    /// admitted extractor marks the pass as relying on optional facts.
    pub(super) const fn admit(&mut self) -> bool {
        match self.policy {
            OptionalFacts::Recorded => {
                self.used = true;
                true
            }
            OptionalFacts::Omitted => false,
        }
    }

    /// Whether this pass records optional facts, without marking it as used.
    pub(super) fn records(self) -> bool {
        self.policy == OptionalFacts::Recorded
    }

    /// Whether an optional extractor ran in this pass, so an output-limit
    /// failure of the pass may be caused by optional facts.
    pub(super) const fn recorded_any(self) -> bool {
        matches!(self.policy, OptionalFacts::Recorded) && self.used
    }
}

/// A walked file and whether its pass recorded optional facts.
pub(crate) struct WalkedFile {
    pub(super) file: ExtractedFile,
    pub(super) recorded_optional_facts: bool,
}

impl WalkedFile {
    /// Run the stages that follow the walk and share its output limit. A
    /// failure keeps whether this pass recorded optional facts.
    pub(crate) fn then(
        self,
        stages: impl FnOnce(ExtractedFile) -> Result<ExtractedFile, ExtractError>,
    ) -> Result<ExtractedFile, PassFailure> {
        let recorded_optional_facts = self.recorded_optional_facts;
        stages(self.file).map_err(|error| PassFailure {
            error,
            recorded_optional_facts,
        })
    }
}

/// A failed pass, and whether it recorded optional facts.
pub(crate) struct PassFailure {
    pub(super) error: ExtractError,
    pub(super) recorded_optional_facts: bool,
}

impl PassFailure {
    /// Whether a fallback pass without optional facts may succeed: only an
    /// output-limit failure of a pass that recorded them can be caused by them.
    fn retries_without_optional_facts(&self) -> bool {
        self.error == ExtractError::OutputLimit && self.recorded_optional_facts
    }
}

/// Run one extraction pass with optional facts recorded and, when that pass
/// failed only because they exceeded the output limit, one more pass without
/// them. The first pass's state is dropped before the fallback runs.
pub(crate) fn extract_with_optional_fact_fallback(
    snapshot: &SourceSnapshot,
    mut pass: impl FnMut(OptionalFacts) -> Result<ExtractedFile, PassFailure>,
) -> Result<ExtractedFile, ExtractError> {
    match pass(OptionalFacts::Recorded) {
        Err(failure) if failure.retries_without_optional_facts() => {
            let file = pass(OptionalFacts::Omitted).map_err(|failure| failure.error)?;
            note_omission(snapshot, file)
        }
        result => result.map_err(|failure| failure.error),
    }
}

/// Say that a fallback pass omitted the file's optional facts, when the
/// diagnostic fits the file's output limit. A file whose remaining facts
/// leave no room for it keeps those facts without the diagnostic rather than
/// failing its generation.
fn note_omission(
    snapshot: &SourceSnapshot,
    mut file: ExtractedFile,
) -> Result<ExtractedFile, ExtractError> {
    let output_limit =
        native_output_limit(snapshot.byte_size()).ok_or(ExtractError::OutputLimit)?;
    note_optional_omission(&mut file, output_limit);
    Ok(file)
}

pub(crate) fn note_optional_omission(file: &mut ExtractedFile, output_limit: u64) {
    if file.diagnostics.try_reserve_exact(1).is_err() {
        return;
    }
    file.diagnostics.push(ExtractionDiagnostic {
        code: DiagnosticCode::OptionalFactsOmitted,
        span: None,
    });
    if file.modeled_retained_bytes() > output_limit {
        file.diagnostics.pop();
        file.diagnostics.shrink_to_fit();
    }
}
