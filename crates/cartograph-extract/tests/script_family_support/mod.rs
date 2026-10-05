//! Helpers shared by the Ruby, Lua, R, and Nix extraction contract tests.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
/// Widths of the smaller input in each linear-work comparison. The larger
/// width keeps the doubled input within the per-file output budget while
/// making per-name rework dominate fixed costs; the smaller one keeps input
/// that nests once per unit of width (parenthesized chains) below the
/// walker's depth limit, beyond which deeper syntax is not analysed at all.
const LINEAR_WORK_BASE_SIZES: [usize; 2] = [24, 150];
/// Largest work growth accepted when an input doubles: linear work about
/// doubles, while work quadratic in the input about quadruples.
const LINEAR_WORK_MAXIMUM_GROWTH: usize = 3;

/// Extract `source` as the file at `path`, panicking with context on failure.
pub fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

/// Cancellation polls made while extracting `source` as `path`: a
/// deterministic measure of how much syntax the walker analysed.
fn extraction_polls(path: &str, source: &str) -> usize {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    let mut polls = 0_usize;
    extractor
        .extract_with_cancellation(&snapshot, || {
            polls = polls.saturating_add(1);
            false
        })
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"));
    polls
}

/// Assert that extracting the input built by `source_of(width)` does work
/// linear in `width`: doubling the width must not come near quadrupling the
/// syntax analysed, as re-analysing a shared declaration (or every enclosing
/// link of a chain) once per name would.
pub fn assert_linear_work(path: &str, source_of: impl Fn(usize) -> String) {
    for width in LINEAR_WORK_BASE_SIZES {
        let base = extraction_polls(path, &source_of(width));
        let doubled = extraction_polls(path, &source_of(width * 2));
        assert!(
            doubled < base.saturating_mul(LINEAR_WORK_MAXIMUM_GROWTH),
            "{path}: work grew from {base} to {doubled} polls when the input doubled from {width}"
        );
    }
}

/// Names `prefix0` through `prefix{count - 1}` joined by `separator`.
pub fn numbered_names(prefix: &str, count: usize, separator: &str) -> String {
    (0..count)
        .map(|index| format!("{prefix}{index}"))
        .collect::<Vec<_>>()
        .join(separator)
}

/// The one symbol of `kind` named `name`, listing what was extracted otherwise.
pub fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| {
            let available = extracted
                .symbols
                .iter()
                .map(|symbol| format!("{:?} {}", symbol.kind, symbol.qualified_name))
                .collect::<Vec<_>>();
            panic!("missing {kind:?} {name}; extracted: {available:?}")
        })
}

/// Sorted names of every symbol of `kind`.
pub fn names_of_kind(extracted: &ExtractedFile, kind: SymbolKind) -> Vec<&str> {
    let mut names = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// An expected reference: its kind, exact name, and owning symbol.
#[derive(Clone, Copy)]
pub struct ReferenceQuery<'query> {
    kind: ReferenceKind,
    name: &'query str,
    owner: Option<&'query SymbolId>,
}

impl<'query> ReferenceQuery<'query> {
    /// An owner-less reference of `kind` named `name`.
    pub const fn new(kind: ReferenceKind, name: &'query str) -> Self {
        Self {
            kind,
            name,
            owner: None,
        }
    }

    /// The same reference owned by `owner`.
    pub const fn owned_by(self, owner: &'query SymbolId) -> Self {
        Self {
            owner: Some(owner),
            ..self
        }
    }

    /// Whether `extracted` retains exactly this kind, name, and owner.
    pub fn found_in(self, extracted: &ExtractedFile) -> bool {
        extracted.references.iter().any(|reference| {
            reference.kind == self.kind
                && reference.name == self.name
                && reference.owner.as_ref() == self.owner
        })
    }

    /// How many retained references match this kind, name, and owner.
    pub fn count_in(self, extracted: &ExtractedFile) -> usize {
        extracted
            .references
            .iter()
            .filter(|reference| {
                reference.kind == self.kind
                    && reference.name == self.name
                    && reference.owner.as_ref() == self.owner
            })
            .count()
    }
}
