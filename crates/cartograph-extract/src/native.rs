use std::ops::ControlFlow;

use cartograph_domain::{FileParseStatus, SourceLanguage};
use thiserror::Error;
use tree_sitter::{ParseOptions, Parser, Point, Query, Tree};

use crate::{
    DiagnosticCode, ExtractedFile, ExtractionDiagnostic, ExtractionStrategy, LanguageSpec,
    SourceSnapshot, custom, framework, objc_macro_rewrite, tags, test_names, walk,
};

/// Default defensive AST depth used when a project does not override it.
pub const DEFAULT_MAXIMUM_AST_DEPTH: usize = 256;
/// Smallest accepted project AST-depth override.
pub const MINIMUM_AST_DEPTH: usize = 64;
/// Hard ceiling for project AST-depth overrides.
pub const MAXIMUM_AST_DEPTH: usize = 1_024;

/// Reusable one-language native parser. Create one per bounded worker.
pub struct NativeExtractor {
    language: SourceLanguage,
    strategy: ExtractionStrategy,
    tags_query: Option<&'static Query>,
    parser: Option<Parser>,
    maximum_ast_depth: usize,
}

impl NativeExtractor {
    /// Load one production-admitted native grammar and reject ABI mismatch.
    /// # Errors
    ///
    /// Returns an error if `language` is not production-admitted or its
    /// implemented grammar/query cannot be loaded with the required ABI.
    pub fn new(language: SourceLanguage) -> Result<Self, ExtractError> {
        if !language.is_native_indexable() {
            return Err(ExtractError::UnsupportedLanguage);
        }
        Self::new_for_capability_validation(language)
    }

    /// Load an implemented extractor before production admission.
    ///
    /// This constructor exists so a language family can be tested and reviewed without making
    /// it importable or indexable. Production discovery and indexing must use [`Self::new`]; the
    /// registry admits a mode only after extraction, cross-file resolution, publication, and
    /// retrieval gates all pass.
    /// # Errors
    ///
    /// Returns an error if no executable strategy exists or the language's
    /// grammar, tags query, or parser ABI cannot be initialized.
    pub fn new_for_capability_validation(language: SourceLanguage) -> Result<Self, ExtractError> {
        let spec = LanguageSpec::for_language(language);
        if !spec.strategy().is_executable() {
            return Err(ExtractError::UnsupportedLanguage);
        }
        let (tags_query, parser) = if spec.strategy() == ExtractionStrategy::CustomStructural {
            (None, None)
        } else {
            let grammar = spec
                .grammar()
                .ok_or(ExtractError::UnsupportedLanguage)?
                .language();
            let tags_query = if spec.strategy() == ExtractionStrategy::TagsQuery {
                Some(tags::query_for(language, &grammar)?)
            } else {
                None
            };
            let mut parser = Parser::new();
            parser
                .set_language(&grammar)
                .map_err(|_| ExtractError::GrammarUnavailable)?;
            (tags_query, Some(parser))
        };
        Ok(Self {
            language,
            strategy: spec.strategy(),
            tags_query,
            parser,
            maximum_ast_depth: DEFAULT_MAXIMUM_AST_DEPTH,
        })
    }

    /// Apply a bounded project-specific AST nesting ceiling.
    /// # Errors
    ///
    /// Returns [`ExtractError::InvalidNestingLimit`] outside 64..=1024.
    pub fn with_maximum_ast_depth(mut self, value: usize) -> Result<Self, ExtractError> {
        if !(MINIMUM_AST_DEPTH..=MAXIMUM_AST_DEPTH).contains(&value) {
            return Err(ExtractError::InvalidNestingLimit);
        }
        self.maximum_ast_depth = value;
        Ok(self)
    }

    /// Extract one immutable snapshot without an external cancellation probe.
    /// # Errors
    ///
    /// Returns an error if snapshot language differs, parsing fails, or custom,
    /// framework, tag, reference, or test-name extraction fails.
    pub fn extract(&mut self, snapshot: &SourceSnapshot) -> Result<ExtractedFile, ExtractError> {
        self.extract_with_cancellation(snapshot, || false)
    }

    /// Extract one snapshot while polling a supervisor-owned cancellation probe.
    /// # Errors
    ///
    /// Returns an error on cancellation, language mismatch, parser failure, or
    /// failed custom/framework/tag/reference/test-name enrichment.
    pub fn extract_with_cancellation(
        &mut self,
        snapshot: &SourceSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<ExtractedFile, ExtractError> {
        match self.extract_with_cancellation_inner(snapshot, &mut cancelled) {
            Err(error) if !cancelled() => recover_file_local_failure(snapshot, error),
            outcome => outcome,
        }
    }

    fn extract_with_cancellation_inner(
        &mut self,
        snapshot: &SourceSnapshot,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<ExtractedFile, ExtractError> {
        if snapshot.language() != self.language {
            return Err(ExtractError::LanguageMismatch);
        }
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if self.strategy == ExtractionStrategy::CustomStructural || custom::scans_snapshot(snapshot)
        {
            let extracted = custom::extract(snapshot, self.maximum_ast_depth, cancelled)?;
            let extracted = framework::enrich(
                framework::FrameworkInput::new(snapshot, extracted),
                cancelled,
            )?;
            return test_names::enrich(snapshot, extracted);
        }

        let rewritten = pre_parse_rewrite(snapshot, cancelled)?;
        let source = rewritten
            .as_deref()
            .unwrap_or_else(|| snapshot.source())
            .as_bytes();
        let tree = parse_with_cancellation(
            self.parser
                .as_mut()
                .ok_or(ExtractError::GrammarUnavailable)?,
            source,
            cancelled,
        )?;
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }

        let root = tree.root_node();
        let parse_status = if root.has_error() {
            FileParseStatus::Partial
        } else {
            FileParseStatus::Parsed
        };
        if self.strategy == ExtractionStrategy::TagsQuery {
            let extracted = tags::extract(
                tags::TagExtractionInput {
                    snapshot,
                    root,
                    parse_status,
                    query: self.tags_query.ok_or(ExtractError::GrammarUnavailable)?,
                },
                cancelled,
            )?;
            let extracted = framework::enrich(
                framework::FrameworkInput::new(snapshot, extracted).with_root(root),
                cancelled,
            )?;
            return test_names::enrich(snapshot, extracted);
        }
        let mut input = walk::WalkInput::new(root, parse_status, self.maximum_ast_depth);
        if let Some(syntax_source) = rewritten.as_deref() {
            input = input.with_syntax_source(syntax_source);
        }
        enrich_walked_file(snapshot, (root, input), cancelled)
    }
}

/// Enrich the walked file with its native syntax root.
fn enrich_walked_file<'source>(
    snapshot: &'source SourceSnapshot,
    (root, input): (tree_sitter::Node<'source>, walk::WalkInput<'source>),
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    walk::extract_with_optional_fact_fallback(snapshot, |optional_facts| {
        walk::extract(
            snapshot,
            input.with_optional_facts(optional_facts),
            &mut *cancelled,
        )?
        .then(|extracted| {
            let extracted = framework::enrich(
                framework::FrameworkInput::new(snapshot, extracted).with_root(root),
                &mut *cancelled,
            )?;
            test_names::enrich(snapshot, extracted)
        })
    })
}

/// Span-preserving text the parser and walker read in place of the snapshot
/// when the language needs a pre-parse rewrite; framework enrichment and test
/// naming keep reading the original snapshot at the same offsets.
fn pre_parse_rewrite(
    snapshot: &SourceSnapshot,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<String>, ExtractError> {
    if snapshot.language() != SourceLanguage::ObjectiveC {
        return Ok(None);
    }
    objc_macro_rewrite::rewrite_react_native_macros(snapshot.source(), cancelled)
}

/// Parse `source` with `parser` while polling the supervisor cancellation probe.
///
/// The parser keeps any included ranges its caller configured, so an embedded
/// script region is parsed over the full host bytes with host positions.
/// # Errors
///
/// Returns [`ExtractError::Cancelled`] when the probe stopped the parse, or
/// [`ExtractError::ParserStopped`] when tree-sitter stopped without a tree.
pub(crate) fn parse_with_cancellation(
    parser: &mut Parser,
    source: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Tree, ExtractError> {
    let mut interrupted = false;
    let tree = {
        let mut input = |offset: usize, _position: Point| match source.get(offset..) {
            Some(remaining) => remaining,
            None => &[],
        };
        let mut progress = |_state: &tree_sitter::ParseState| {
            if cancelled() {
                interrupted = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        parser.parse_with_options(&mut input, None, Some(options))
    };
    tree.ok_or_else(|| {
        parser.reset();
        if interrupted {
            ExtractError::Cancelled
        } else {
            ExtractError::ParserStopped
        }
    })
}

fn recover_file_local_failure(
    snapshot: &SourceSnapshot,
    error: ExtractError,
) -> Result<ExtractedFile, ExtractError> {
    let diagnostic = match error {
        ExtractError::NestingLimit => DiagnosticCode::NestingLimitExceeded,
        ExtractError::InvalidSpan => DiagnosticCode::InvalidSpan,
        ExtractError::ParserStopped => DiagnosticCode::ParserStopped,
        _ => return Err(error),
    };
    Ok(degraded_file(snapshot, diagnostic))
}

fn degraded_file(snapshot: &SourceSnapshot, diagnostic: DiagnosticCode) -> ExtractedFile {
    ExtractedFile {
        file_id: snapshot.file_id().clone(),
        path: snapshot.path().clone(),
        language: snapshot.language(),
        content_hash: snapshot.content_hash().clone(),
        byte_size: snapshot.byte_size(),
        line_count: snapshot.line_count(),
        parse_status: FileParseStatus::Partial,
        symbols: Vec::new(),
        containments: Vec::new(),
        references: Vec::new(),
        call_scope_sites: Vec::new(),
        javascript_member_calls: Vec::new(),
        numerical_sites: Vec::new(),
        import_bindings: Vec::new(),
        has_inline_tests: false,
        test_search_text: String::new(),
        test_search_truncated: false,
        diagnostics: vec![ExtractionDiagnostic {
            code: diagnostic,
            span: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceLimits;

    #[test]
    fn invalid_span_is_a_partial_file_instead_of_a_generation_fatal_error() {
        let limits =
            SourceLimits::new(1024).unwrap_or_else(|error| panic!("source limits failed: {error}"));
        let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
            "shader.slang",
            b"void main() {}\n",
            limits,
        )
        .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
        let recovered = recover_file_local_failure(&snapshot, ExtractError::InvalidSpan)
            .unwrap_or_else(|error| panic!("invalid span stayed fatal: {error}"));
        assert_eq!(recovered.parse_status, FileParseStatus::Partial);
        assert_eq!(recovered.symbols, []);
        assert_eq!(
            recovered.diagnostics,
            vec![ExtractionDiagnostic {
                code: DiagnosticCode::InvalidSpan,
                span: None,
            }]
        );
        let stopped = recover_file_local_failure(&snapshot, ExtractError::ParserStopped)
            .unwrap_or_else(|error| panic!("parser stop stayed fatal: {error}"));
        assert_eq!(
            stopped.diagnostics,
            vec![ExtractionDiagnostic {
                code: DiagnosticCode::ParserStopped,
                span: None,
            }]
        );
    }

    #[test]
    fn systemic_and_cancelled_failures_remain_fatal() {
        let limits =
            SourceLimits::new(1024).unwrap_or_else(|error| panic!("source limits failed: {error}"));
        let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
            "shader.slang",
            b"void main() {}\n",
            limits,
        )
        .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
        for error in [
            ExtractError::Cancelled,
            ExtractError::GrammarUnavailable,
            ExtractError::OutputLimit,
        ] {
            assert_eq!(recover_file_local_failure(&snapshot, error), Err(error));
        }
    }
}

/// Credential-safe native grammar, cancellation, or source-boundary failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ExtractError {
    /// No executable extractor exists, or production admission has not been granted.
    #[error("source language is not available through this native extractor entry point")]
    UnsupportedLanguage,
    /// The reusable parser was called with a snapshot for another grammar.
    #[error("native parser language does not match the source snapshot")]
    LanguageMismatch,
    /// A statically linked grammar was incompatible with the parser ABI.
    #[error("native grammar is unavailable")]
    GrammarUnavailable,
    /// Tree-sitter stopped without a supervisor cancellation request.
    #[error("native parser stopped before producing a syntax tree")]
    ParserStopped,
    /// The supervisor requested cancellation before extraction completed.
    #[error("native extraction was cancelled")]
    Cancelled,
    /// A parser offset could not fit the durable source-span contract.
    #[error("native parser produced an invalid source span")]
    InvalidSpan,
    /// A source syntax tree exceeded the defensive nesting ceiling.
    #[error("native source nesting exceeds the extraction limit")]
    NestingLimit,
    /// A caller supplied an AST-depth policy outside the hard safety range.
    #[error("native extraction nesting limit is outside the supported range")]
    InvalidNestingLimit,
    /// Extracted facts exceeded the per-file fact, string, or modeled-output bound.
    #[error("native extraction output exceeds the configured bound")]
    OutputLimit,
}
