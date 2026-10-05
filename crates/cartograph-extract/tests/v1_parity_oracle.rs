//! Per-language v1.1.33 parity oracle for native extraction.
//!
//! Every v1.1.33 language mode owns a fixture corpus in
//! `tests/fixtures/v1_parity/<language>/` and the facts the released v1.1.33
//! binary indexed from that corpus in
//! `tests/fixtures/v1_parity/expected/<language>.json`, captured by the
//! maintainer-only `scripts/capture-v1-parity-oracle.sh`. This target runs the v2
//! [`NativeExtractor`] over every corpus file, with the corpus-relative path as
//! the virtual path, and requires every v1 fact to have a v2 counterpart under the
//! rules below or an entry in the divergence ledger
//! `tests/fixtures/v1_parity/divergences.json`.
//!
//! # Language attribution
//!
//! The capture does not record v1's per-file language. A file is attributed to a
//! mode when v1 emitted a file node for it and the frozen v1.1.33 classifier port,
//! `SourceLanguage::for_v1_normalized_path_with_source`, selects that mode. The v2
//! production classifier behind `SourceSnapshot::from_bytes` must then select the
//! same mode. Corpora may hold files of other languages, such as a bridge caller or
//! a framework manifest, and their v1 facts are checked too.
//!
//! # Mapping rules
//!
//! Two names *agree* when they are equal or share a non-empty final segment. The
//! final segment is the text after the last `::`, `.`, `\` or `/`. No other fuzzy
//! matching is applied. Cross-file target identity is not asserted here; that
//! belongs to the resolution oracle. Only the presence of the per-file fact is.
//!
//! * **Symbols.** A v1 `file` node is the file itself and always matches. A v1
//!   `import` node matches a v2 `import` symbol, import binding or `imports`
//!   reference whose module specifier equals the node name, located within the
//!   node's lines. Every other node matches a v2 symbol with the kind listed in
//!   [`SYMBOL_KIND_MAP`], an equal name, the same 1-based start line and an agreeing
//!   qualified name. A v1 qualified name scoped by the node's own file path
//!   (`<file>::…` or `<file>#…`, as for routes and framework resources) is a
//!   synthetic identity and is not compared. When both qualified names use the same
//!   primary separator (the first of `::`, `.`, `\`, `/` they contain), the v2 name
//!   must equal the v1 name or end with it after a separator, which admits a v2
//!   package prefix. Otherwise only their final segments must be equal.
//! * **`contains` edges.** A file parent matches a v2 symbol with the child's name
//!   that is top-level or nested only in v2 `namespace` symbols, because v2 models
//!   package and namespace declarations that v1 left implicit. Any other parent
//!   matches a v2 containment whose parent and child carry the v1 names. An import
//!   child matches when its import node matches.
//! * **Other edges.** A v2 reference in the edge's source file with a kind listed
//!   in [`EDGE_KIND_MAP`], starting on the v1 line when v1 recorded one, whose name
//!   agrees with the target node's name or whose segment before the first separator
//!   (the receiver of a member access, such as `Ctx` in `Ctx.Provider`) equals it.
//!   When the target is an import node, a reference whose name, or that leading
//!   segment, is the local name of an import binding from that module also matches,
//!   and a `references` edge recorded on the import node's own lines matches the
//!   import itself. An `imports` edge to a non-import target matches an `imports`
//!   reference, import binding or `import` symbol on that line whose specifier, path
//!   leaf, leaf stem or final segment equals the target name or its stem.
//! * **Unresolved references.** A v2 reference with the mapped kind, starting on
//!   that line, whose name agrees with the v1 name.
//!
//! # Divergence ledger
//!
//! Each ledger entry names one v1 fact by language, category, kind, name (the
//! target's qualified name for edges), source (edges only), file and line, with a
//! `pending` or `intentional` status, an optional audit gap id and wave, and a
//! reason that `intentional` entries must fill. The oracle fails when a v1 fact
//! has neither a v2 match nor an entry, and when an entry names a fact that now
//! matches or no longer exists, so the ledger can only shrink honestly. Set
//! `CARTOGRAPH_V1_PARITY_REPORT` to a file path to write every unmatched v1 fact as
//! JSON in the ledger's fact shape.

mod dependency_ownership;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::Path,
};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};
use serde::{Deserialize, Serialize};

/// Root of the per-language corpora, the captured oracles and the ledger.
const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1_parity");
/// Directory under [`FIXTURE_ROOT`] holding one oracle document per mode.
const EXPECTED_DIRECTORY: &str = "expected";
/// Extension of the oracle documents.
const ORACLE_EXTENSION: &str = "json";
/// Divergence ledger file under [`FIXTURE_ROOT`].
const LEDGER_FILE: &str = "divergences.json";
/// Release every oracle document was captured from.
const V1_BASELINE: &str = "v1.1.33";
/// SHA-256 of the v1.1.33 darwin-arm64 `cartograph` binary used for every capture.
const V1_BINARY_SHA256: &str = "c223ca62ad1a31f2686c7e79f7175fde440e7f98135062eb3ed8f09937b7e5c7";
/// Number of language modes in the frozen v1.1.33 registry.
const V1_LANGUAGE_MODE_COUNT: usize = 73;
/// Per-file source bound; the largest corpus file is far below it.
const SOURCE_LIMIT_BYTES: usize = 1024 * 1024;
/// Environment variable naming an optional JSON miss-report path.
const REPORT_PATH_VARIABLE: &str = "CARTOGRAPH_V1_PARITY_REPORT";
/// Separators that end a qualified or member-access name segment.
const NAME_SEPARATORS: [&str; 4] = ["::", ".", "\\", "/"];
/// Separators that follow a file path used as a synthetic qualified-name scope.
const FILE_SCOPE_SEPARATORS: [&str; 2] = ["::", "#"];
/// Separators that end a module-specifier path component.
const PATH_SEPARATORS: [char; 2] = ['/', '\\'];
/// Waves a ledger entry may be scheduled for.
const LEDGER_WAVES: [u8; 3] = [1, 2, 3];
/// v1 node kind for a whole source file.
const V1_FILE_KIND: &str = "file";
/// v1 node kind for an import statement.
const V1_IMPORT_KIND: &str = "import";
/// v1 edge kind for lexical containment.
const V1_CONTAINS_KIND: &str = "contains";
/// v1 edge kind for module imports.
const V1_IMPORTS_KIND: &str = "imports";
/// v1 edge kind for general references.
const V1_REFERENCES_KIND: &str = "references";

/// Modes for which v1.1.33 emitted only file nodes in attributed files.
const PARSER_ONLY_V1_MODES: [SourceLanguage; 7] = [
    SourceLanguage::Css,
    SourceLanguage::EmbeddedTemplate,
    SourceLanguage::Html,
    SourceLanguage::JsDoc,
    SourceLanguage::Json,
    SourceLanguage::Jupyter,
    SourceLanguage::Regex,
];

/// v1 node kind to v2 symbol kind. Every v1.1.33 kind kept its stable spelling;
/// `file` and `import` nodes follow their own rules and are not listed.
const SYMBOL_KIND_MAP: [(&str, SymbolKind); 22] = [
    ("class", SymbolKind::Class),
    ("component", SymbolKind::Component),
    ("constant", SymbolKind::Constant),
    ("enum", SymbolKind::Enum),
    ("enum_member", SymbolKind::EnumMember),
    ("export", SymbolKind::Export),
    ("field", SymbolKind::Field),
    ("function", SymbolKind::Function),
    ("interface", SymbolKind::Interface),
    ("method", SymbolKind::Method),
    ("module", SymbolKind::Module),
    ("namespace", SymbolKind::Namespace),
    ("parameter", SymbolKind::Parameter),
    ("property", SymbolKind::Property),
    ("protocol", SymbolKind::Protocol),
    ("resource", SymbolKind::Resource),
    ("route", SymbolKind::Route),
    ("struct", SymbolKind::Struct),
    ("table", SymbolKind::Table),
    ("trait", SymbolKind::Trait),
    ("type_alias", SymbolKind::TypeAlias),
    ("variable", SymbolKind::Variable),
];

/// v1 edge or unresolved-reference kind to the v2 reference kinds that carry it.
/// v2 `inherits` defers the class/interface role of a base type to resolution.
const EDGE_KIND_MAP: [(&str, &[ReferenceKind]); 14] = [
    ("calls", &[ReferenceKind::Calls]),
    ("decorates", &[ReferenceKind::Decorates]),
    ("def_use", &[ReferenceKind::DefUse]),
    ("exports", &[ReferenceKind::Exports]),
    (
        "extends",
        &[ReferenceKind::Extends, ReferenceKind::Inherits],
    ),
    ("field_access", &[ReferenceKind::FieldAccess]),
    (
        "implements",
        &[ReferenceKind::Implements, ReferenceKind::Inherits],
    ),
    ("imports", &[ReferenceKind::Imports]),
    ("instantiates", &[ReferenceKind::Instantiates]),
    ("overrides", &[ReferenceKind::Overrides]),
    ("references", &[ReferenceKind::References]),
    ("returns", &[ReferenceKind::Returns]),
    ("tests", &[ReferenceKind::Tests]),
    ("type_of", &[ReferenceKind::TypeOf]),
];

/// One captured v1.1.33 oracle document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Oracle {
    /// Release the facts were captured from.
    baseline: String,
    /// SHA-256 of the v1 binary that produced the capture.
    binary_sha256: String,
    /// Stable id of the mode the corpus exercises.
    language: String,
    /// Corpus files, excluding dotfiles, in byte order.
    files: Vec<String>,
    /// Every v1 node, including file and import nodes.
    symbols: Vec<V1Symbol>,
    /// Every resolved v1 edge.
    edges: Vec<V1Edge>,
    /// References v1 left unresolved after resolution.
    unresolved: Vec<V1Unresolved>,
}

/// One v1 graph node.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Symbol {
    /// Corpus-relative file path.
    file: String,
    /// v1 node kind.
    kind: String,
    /// Display name.
    name: String,
    /// v1 qualified name; the path for file nodes.
    qualified_name: String,
    /// 1-based first line.
    start_line: u32,
    /// 1-based last line.
    end_line: u32,
}

/// One resolved v1 edge between two nodes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Edge {
    /// Source node's corpus-relative file path.
    file: String,
    /// v1 edge kind.
    kind: String,
    /// Source node kind.
    source_kind: String,
    /// Source node qualified name.
    source: String,
    /// Target node kind.
    target_kind: String,
    /// Target node qualified name.
    target: String,
    /// Target node's corpus-relative file path.
    target_file: String,
    /// 1-based reference line, absent for containment and file-level facts.
    line: Option<u32>,
}

/// One reference v1 could not resolve.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Unresolved {
    /// Corpus-relative file path.
    file: String,
    /// v1 reference kind.
    kind: String,
    /// Referenced name.
    name: String,
    /// 1-based reference line.
    line: Option<u32>,
}

/// Category of a v1 fact.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum FactCategory {
    /// A v1 node.
    Symbol,
    /// A resolved v1 edge.
    Edge,
    /// An unresolved v1 reference.
    Unresolved,
}

/// Stable identity of one v1 fact, shared by the ledger and the miss report.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct FactKey {
    /// Fact category.
    category: FactCategory,
    /// File the fact belongs to: the node file, or the edge's source file.
    file: String,
    /// v1 line: node start line, or edge/reference line when recorded.
    line: Option<u32>,
    /// v1 node, edge or reference kind.
    kind: String,
    /// Node or reference name; the target qualified name for edges.
    name: String,
    /// Source qualified name, present for edges only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

/// Lifecycle of a ledger entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LedgerStatus {
    /// The fact is expected to return when its port lands.
    Pending,
    /// v2 deliberately differs; the reason explains why.
    Intentional,
}

/// One accepted divergence from a v1 fact.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerEntry {
    /// Stable id of the oracle mode the fact belongs to.
    language: String,
    /// The v1 fact without a v2 match.
    fact: FactKey,
    /// Pending port or intentional difference.
    status: LedgerStatus,
    /// Audit gap id tracking the port.
    #[serde(default)]
    gap: Option<String>,
    /// Port wave the fact is scheduled for.
    #[serde(default)]
    wave: Option<u8>,
    /// Why the fact diverges; required for intentional entries.
    reason: String,
}

/// One row of the optional machine-readable miss report.
#[derive(Serialize)]
struct MissRecord<'a> {
    /// Stable id of the oracle mode.
    language: &'a str,
    /// The unmatched v1 fact.
    #[serde(flatten)]
    fact: &'a FactKey,
}

/// One corpus file read from disk.
struct CorpusFile {
    /// Corpus-relative path with `/` separators.
    path: String,
    /// Exact file bytes.
    bytes: Vec<u8>,
}

/// One oracle document with its corpus.
struct LanguageCase {
    /// The mode the oracle exercises.
    language: SourceLanguage,
    /// Captured v1 facts.
    oracle: Oracle,
    /// Every corpus file, including dotfiles.
    corpus: Vec<CorpusFile>,
}

/// v2 classification and extraction of one corpus file.
struct FileRun {
    /// v2 production language, when the file could be snapshotted.
    detected: Option<SourceLanguage>,
    /// Extracted facts, or the reason none exist.
    extraction: Result<ExtractedFile, String>,
}

/// Unmatched facts and integrity findings for one language.
#[derive(Default)]
struct LanguageOutcome {
    /// v1 facts without a v2 match.
    misses: BTreeSet<FactKey>,
    /// Failures that no ledger entry can excuse.
    problems: Vec<String>,
    /// Context printed with failures, such as extraction errors.
    notes: Vec<String>,
}

/// Ledger reconciliation result for one language.
#[derive(Default)]
struct LanguageFailures {
    /// Unmatched facts with no ledger entry.
    unledgered: Vec<FactKey>,
    /// Ledger entries whose fact matches or no longer exists.
    stale: Vec<FactKey>,
    /// Integrity failures.
    problems: Vec<String>,
    /// Context for the failures above.
    notes: Vec<String>,
}

impl LanguageFailures {
    /// Whether this language contributes nothing to the failure report.
    fn is_clean(&self) -> bool {
        self.unledgered.is_empty() && self.stale.is_empty() && self.problems.is_empty()
    }
}

/// Inclusive 1-based line window a v2 fact must start in.
#[derive(Clone, Copy)]
struct LineSpan {
    /// First accepted line.
    first: u32,
    /// Last accepted line.
    last: u32,
}

impl LineSpan {
    /// Any line of the file.
    const ANY: Self = Self {
        first: 1,
        last: u32::MAX,
    };

    /// Exactly one line.
    const fn exact(line: u32) -> Self {
        Self {
            first: line,
            last: line,
        }
    }

    /// The recorded line, or any line when v1 recorded none.
    fn recorded(line: Option<u32>) -> Self {
        line.map_or(Self::ANY, Self::exact)
    }

    /// Whether `line` lies inside the window.
    const fn contains(self, line: u32) -> bool {
        self.first <= line && line <= self.last
    }
}

/// v1 nodes keyed by file, qualified name and kind, the identity edges carry.
struct V1Nodes<'a> {
    /// Nodes sharing one identity; qualified names are not always unique.
    by_identity: BTreeMap<(&'a str, &'a str, &'a str), Vec<&'a V1Symbol>>,
}

impl<'a> V1Nodes<'a> {
    /// Index every node of one oracle.
    fn new(symbols: &'a [V1Symbol]) -> Self {
        let mut by_identity = BTreeMap::<_, Vec<_>>::new();
        for symbol in symbols {
            by_identity
                .entry((
                    symbol.file.as_str(),
                    symbol.qualified_name.as_str(),
                    symbol.kind.as_str(),
                ))
                .or_default()
                .push(symbol);
        }
        Self { by_identity }
    }

    /// Nodes with one identity; empty when the capture is inconsistent.
    fn find<'s>(&'s self, identity: (&'s str, &'s str, &'s str)) -> &'s [&'a V1Symbol] {
        self.by_identity.get(&identity).map_or(&[], Vec::as_slice)
    }
}

/// v2 facts of one extracted file, indexed for the mapping rules.
struct FileFacts<'a> {
    /// The v2 extraction.
    file: &'a ExtractedFile,
    /// Symbols by id.
    by_id: BTreeMap<&'a str, &'a ExtractedSymbol>,
    /// Direct parent id of each contained symbol id.
    parent_of: BTreeMap<&'a str, &'a str>,
}

/// One edge-to-reference lookup.
struct ReferenceQuery<'q> {
    /// v1 edge kind.
    v1_kind: &'q str,
    /// Accepted v2 reference kinds.
    kinds: &'q [ReferenceKind],
    /// v1 reference line, absent for file-level facts.
    line: Option<u32>,
    /// v1 target node.
    target: &'q V1Symbol,
}

impl<'a> FileFacts<'a> {
    /// Index one extraction.
    fn new(file: &'a ExtractedFile) -> Self {
        Self {
            file,
            by_id: file
                .symbols
                .iter()
                .map(|symbol| (symbol.id.as_str(), symbol))
                .collect(),
            parent_of: file
                .containments
                .iter()
                .map(|edge| (edge.child.as_str(), edge.parent.as_str()))
                .collect(),
        }
    }

    /// Name of a symbol id, if the id is known.
    fn name_of(&self, id: &str) -> Option<&'a str> {
        self.by_id.get(id).map(|symbol| symbol.name.as_str())
    }

    /// Whether an import of `specifier` starts inside `lines`.
    fn has_import(&self, specifier: &str, lines: LineSpan) -> bool {
        self.import_specifiers(lines)
            .any(|found| found == specifier)
    }

    /// Module specifiers of import symbols, bindings and `imports` references in `lines`.
    fn import_specifiers(&self, lines: LineSpan) -> impl Iterator<Item = &'a str> {
        let file = self.file;
        let symbols = file
            .symbols
            .iter()
            .filter(move |symbol| {
                symbol.kind == SymbolKind::Import && lines.contains(symbol.span.start_line())
            })
            .map(|symbol| symbol.name.as_str());
        let bindings = file
            .import_bindings
            .iter()
            .filter(move |binding| lines.contains(binding.span.start_line()))
            .map(|binding| binding.module_specifier.as_str());
        let references = file
            .references
            .iter()
            .filter(move |reference| {
                reference.kind == ReferenceKind::Imports
                    && lines.contains(reference.span.start_line())
            })
            .map(|reference| reference.name.as_str());
        symbols.chain(bindings).chain(references)
    }

    /// Whether `name`, or its leading segment, is bound by an import from `specifier`.
    fn binds_import(&self, name: &str, specifier: &str) -> bool {
        let leading = leading_segment(name);
        self.file.import_bindings.iter().any(|binding| {
            binding.module_specifier == specifier
                && (binding.local_name == name || binding.local_name == leading)
        })
    }

    /// Whether a symbol named `name` is top-level or nested only in namespaces.
    fn has_file_scoped(&self, name: &str) -> bool {
        self.file
            .symbols
            .iter()
            .any(|symbol| symbol.name == name && self.is_file_scoped(symbol.id.as_str()))
    }

    /// Whether every ancestor of `id` is a v2 namespace symbol.
    fn is_file_scoped(&self, id: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.by_id.len() {
            let Some(parent) = self.parent_of.get(current) else {
                return true;
            };
            if self
                .by_id
                .get(parent)
                .is_none_or(|symbol| symbol.kind != SymbolKind::Namespace)
            {
                return false;
            }
            current = parent;
        }
        false
    }

    /// Whether a containment links a parent named `parent` to a child named `child`.
    fn has_containment(&self, parent: &str, child: &str) -> bool {
        self.file.containments.iter().any(|edge| {
            self.name_of(edge.parent.as_str()) == Some(parent)
                && self.name_of(edge.child.as_str()) == Some(child)
        })
    }

    /// Whether a v2 symbol carries the v1 node under the symbol rule.
    fn has_symbol(&self, symbol: &V1Symbol, kind: SymbolKind) -> bool {
        self.file.symbols.iter().any(|candidate| {
            candidate.kind == kind
                && candidate.name == symbol.name
                && candidate.span.start_line() == symbol.start_line
                && qualified_names_agree(symbol, &candidate.qualified_name)
        })
    }

    /// Whether a v2 reference carries a non-`imports` v1 edge.
    fn references_target(&self, query: &ReferenceQuery<'_>) -> bool {
        let lines = LineSpan::recorded(query.line);
        let referenced = self.file.references.iter().any(|reference| {
            query.kinds.contains(&reference.kind)
                && lines.contains(reference.span.start_line())
                && self.reference_names_target(&reference.name, query.target)
        });
        referenced || self.names_imported_binding(query)
    }

    /// Whether a v2 reference name designates a v1 target node: the names agree,
    /// the target is the receiver of a member access, or the target is an import
    /// node whose module binds the name.
    fn reference_names_target(&self, name: &str, target: &V1Symbol) -> bool {
        names_agree(name, &target.name)
            || leading_segment(name) == target.name
            || target.kind == V1_IMPORT_KIND && self.binds_import(name, &target.name)
    }

    /// Whether a v1 `references` edge recorded on an import node's own lines is
    /// carried by that import, which names the imported binding.
    fn names_imported_binding(&self, query: &ReferenceQuery<'_>) -> bool {
        let target = query.target;
        let own_lines = LineSpan {
            first: target.start_line,
            last: target.end_line,
        };
        query.v1_kind == V1_REFERENCES_KIND
            && target.kind == V1_IMPORT_KIND
            && query.line.is_some_and(|line| {
                own_lines.contains(line) && self.has_import(&target.name, LineSpan::exact(line))
            })
    }

    /// Whether a v2 import fact carries a v1 `imports` edge.
    fn imports_target(&self, query: &ReferenceQuery<'_>) -> bool {
        let target = query.target;
        let lines = LineSpan::recorded(query.line);
        if target.kind == V1_IMPORT_KIND {
            return self.has_import(&target.name, lines);
        }
        self.import_specifiers(lines)
            .any(|specifier| specifier_names_target(specifier, target))
    }
}

/// Matches one language's v1 facts against its v2 extractions.
struct Matcher<'a> {
    /// v1 nodes by identity.
    nodes: V1Nodes<'a>,
    /// Indexed v2 facts of every successfully extracted file.
    files: BTreeMap<&'a str, FileFacts<'a>>,
}

impl Matcher<'_> {
    /// Apply the symbol rule.
    fn symbol(&self, symbol: &V1Symbol) -> bool {
        if symbol.kind == V1_FILE_KIND {
            return true;
        }
        let Some(facts) = self.files.get(symbol.file.as_str()) else {
            return false;
        };
        if symbol.kind == V1_IMPORT_KIND {
            let lines = LineSpan {
                first: symbol.start_line,
                last: symbol.end_line,
            };
            return facts.has_import(&symbol.name, lines);
        }
        symbol_kind(&symbol.kind).is_some_and(|kind| facts.has_symbol(symbol, kind))
    }

    /// Apply the `contains` or reference edge rule.
    fn edge(&self, edge: &V1Edge) -> bool {
        let targets = self
            .nodes
            .find((&edge.target_file, &edge.target, &edge.target_kind));
        if edge.kind == V1_CONTAINS_KIND {
            return targets.iter().any(|child| self.containment(edge, child));
        }
        let (Some(kinds), Some(facts)) = (
            reference_kinds(&edge.kind),
            self.files.get(edge.file.as_str()),
        ) else {
            return false;
        };
        targets.iter().any(|target| {
            let query = ReferenceQuery {
                v1_kind: &edge.kind,
                kinds,
                line: edge.line,
                target,
            };
            if edge.kind == V1_IMPORTS_KIND {
                facts.imports_target(&query)
            } else {
                facts.references_target(&query)
            }
        })
    }

    /// Apply the containment rule for one candidate child node.
    fn containment(&self, edge: &V1Edge, child: &V1Symbol) -> bool {
        if child.kind == V1_IMPORT_KIND {
            return self.symbol(child);
        }
        let Some(facts) = self.files.get(child.file.as_str()) else {
            return false;
        };
        if edge.source_kind == V1_FILE_KIND {
            return facts.has_file_scoped(&child.name);
        }
        self.nodes
            .find((&edge.file, &edge.source, &edge.source_kind))
            .iter()
            .any(|parent| facts.has_containment(&parent.name, &child.name))
    }

    /// Apply the unresolved-reference rule.
    fn unresolved(&self, reference: &V1Unresolved) -> bool {
        let (Some(kinds), Some(facts)) = (
            reference_kinds(&reference.kind),
            self.files.get(reference.file.as_str()),
        ) else {
            return false;
        };
        let lines = LineSpan::recorded(reference.line);
        facts.file.references.iter().any(|candidate| {
            kinds.contains(&candidate.kind)
                && lines.contains(candidate.span.start_line())
                && names_agree(&candidate.name, &reference.name)
        })
    }
}

#[test]
fn v2_extraction_carries_every_v1_parity_fact_or_a_ledgered_divergence() {
    let ledger = load_ledger();
    let mut outcomes = BTreeMap::new();
    let mut fact_count = 0_usize;
    for language in v1_languages() {
        let case = load_case(language);
        fact_count +=
            case.oracle.symbols.len() + case.oracle.edges.len() + case.oracle.unresolved.len();
        outcomes.insert(language.as_str().to_owned(), evaluate(&case));
    }
    write_requested_report(&outcomes);
    let failures = reconcile(&outcomes, &ledger);
    assert!(
        failures.values().all(LanguageFailures::is_clean),
        "{}",
        render_failures(&failures, fact_count)
    );
}

#[test]
fn every_v1_language_mode_has_a_complete_parity_corpus() {
    let languages = v1_languages();
    assert_eq!(languages.len(), V1_LANGUAGE_MODE_COUNT);
    let expected_ids = languages
        .iter()
        .map(|language| language.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(oracle_document_ids(), expected_ids);

    let mut problems = Vec::new();
    for language in languages {
        let case = load_case(language);
        for problem in corpus_problems(&case) {
            problems.push(format!("{}: {problem}", language.as_str()));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn divergence_ledger_entries_are_well_formed_and_unique() {
    let ledger = load_ledger();
    let languages = v1_languages()
        .into_iter()
        .map(SourceLanguage::as_str)
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut problems = Vec::new();
    for entry in &ledger {
        let label = format!("{}: {}", entry.language, describe(&entry.fact));
        if !languages.contains(entry.language.as_str()) {
            problems.push(format!("{label}: not a v1 language mode"));
        }
        if entry.status == LedgerStatus::Intentional && entry.reason.trim().is_empty() {
            problems.push(format!("{label}: intentional entries need a reason"));
        }
        if entry.wave.is_some_and(|wave| !LEDGER_WAVES.contains(&wave)) {
            problems.push(format!("{label}: wave must be 1, 2 or 3"));
        }
        if entry
            .gap
            .as_deref()
            .is_some_and(|gap| gap.trim().is_empty())
        {
            problems.push(format!("{label}: gap ids must not be blank"));
        }
        if (entry.fact.category == FactCategory::Edge) != entry.fact.source.is_some() {
            problems.push(format!(
                "{label}: edge facts need a source and other facts take none"
            ));
        }
        if !seen.insert((entry.language.as_str(), &entry.fact)) {
            problems.push(format!("{label}: duplicate entry"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every v1.1.33 language mode, in stable order.
fn v1_languages() -> Vec<SourceLanguage> {
    SourceLanguage::ALL
        .into_iter()
        .filter(|language| language.is_v1_language())
        .collect()
}

/// Stable ids of every oracle document on disk.
fn oracle_document_ids() -> BTreeSet<String> {
    let directory = Path::new(FIXTURE_ROOT).join(EXPECTED_DIRECTORY);
    let entries = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display()));
    entries
        .map(|entry| entry.unwrap_or_else(|error| panic!("cannot read oracle entry: {error}")))
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == ORACLE_EXTENSION)
        })
        .filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect()
}

/// Load one mode's oracle document and corpus.
fn load_case(language: SourceLanguage) -> LanguageCase {
    let path = Path::new(FIXTURE_ROOT)
        .join(EXPECTED_DIRECTORY)
        .join(format!("{}.{ORACLE_EXTENSION}", language.as_str()));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let oracle = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not a valid oracle: {error}", path.display()));
    LanguageCase {
        language,
        oracle,
        corpus: corpus_files(&Path::new(FIXTURE_ROOT).join(language.as_str())),
    }
}

/// Parse the divergence ledger.
fn load_ledger() -> Vec<LedgerEntry> {
    let path = Path::new(FIXTURE_ROOT).join(LEDGER_FILE);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not a valid ledger: {error}", path.display()))
}

/// Every file under `root`, including dotfiles, sorted by corpus-relative path.
fn corpus_files(root: &Path) -> Vec<CorpusFile> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|error| panic!("cannot read corpus entry: {error}"))
                .path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(read_corpus_file(root, &path));
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

/// Read one corpus file and derive its corpus-relative path.
fn read_corpus_file(root: &Path, path: &Path) -> CorpusFile {
    let relative = path
        .strip_prefix(root)
        .unwrap_or_else(|error| panic!("{} escaped its corpus: {error}", path.display()));
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    CorpusFile {
        path: components.join("/"),
        bytes: fs::read(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display())),
    }
}

/// Paths v1 emitted a file node for.
fn v1_file_nodes(oracle: &Oracle) -> BTreeSet<&str> {
    oracle
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == V1_FILE_KIND)
        .map(|symbol| symbol.file.as_str())
        .collect()
}

/// Corpus files v1 indexed that the frozen v1 classifier assigns to the case's mode.
fn attributed_files(case: &LanguageCase) -> BTreeSet<&str> {
    let indexed = v1_file_nodes(&case.oracle);
    case.corpus
        .iter()
        .filter(|file| indexed.contains(file.path.as_str()))
        .filter(|file| {
            std::str::from_utf8(&file.bytes).ok().and_then(|text| {
                SourceLanguage::for_v1_normalized_path_with_source(&file.path, text)
            }) == Some(case.language)
        })
        .map(|file| file.path.as_str())
        .collect()
}

/// Classify and extract one corpus file through the v2 production entry points.
fn run_file(file: &CorpusFile) -> FileRun {
    let limits = SourceLimits::new(SOURCE_LIMIT_BYTES)
        .unwrap_or_else(|error| panic!("source limit is invalid: {error}"));
    let snapshot = match SourceSnapshot::from_bytes(&file.path, &file.bytes, limits) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return FileRun {
                detected: None,
                extraction: Err(format!("snapshot rejected: {error}")),
            };
        }
    };
    let extraction = NativeExtractor::new(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .map_err(|error| format!("extraction failed: {error}"));
    FileRun {
        detected: Some(snapshot.language()),
        extraction,
    }
}

/// Run v2 over one corpus and match every v1 fact.
fn evaluate(case: &LanguageCase) -> LanguageOutcome {
    let mut outcome = LanguageOutcome::default();
    let runs = case
        .corpus
        .iter()
        .map(|file| (file.path.as_str(), run_file(file)))
        .collect::<BTreeMap<_, _>>();
    let indexed = v1_file_nodes(&case.oracle);
    for path in attributed_files(case) {
        let detected = runs.get(path).and_then(|run| run.detected);
        if detected != Some(case.language) {
            outcome.problems.push(format!(
                "{path}: v1 attributed it to {} but v2 classifies it as {}",
                case.language.as_str(),
                detected.map_or("nothing", SourceLanguage::as_str)
            ));
        }
    }
    for (path, run) in &runs {
        if let Err(reason) = &run.extraction
            && indexed.contains(path)
        {
            outcome.notes.push(format!("{path}: {reason}"));
        }
    }
    let matcher = Matcher {
        nodes: V1Nodes::new(&case.oracle.symbols),
        files: runs
            .iter()
            .filter_map(|(path, run)| {
                run.extraction
                    .as_ref()
                    .ok()
                    .map(|file| (*path, FileFacts::new(file)))
            })
            .collect(),
    };
    outcome.misses = unmatched_facts(&case.oracle, &matcher);
    outcome
}

/// Every v1 fact of one oracle that has no v2 match.
fn unmatched_facts(oracle: &Oracle, matcher: &Matcher<'_>) -> BTreeSet<FactKey> {
    let symbols = oracle
        .symbols
        .iter()
        .filter(|symbol| !matcher.symbol(symbol))
        .map(|symbol| FactKey {
            category: FactCategory::Symbol,
            file: symbol.file.clone(),
            line: Some(symbol.start_line),
            kind: symbol.kind.clone(),
            name: symbol.name.clone(),
            source: None,
        });
    let edges = oracle
        .edges
        .iter()
        .filter(|edge| !matcher.edge(edge))
        .map(|edge| FactKey {
            category: FactCategory::Edge,
            file: edge.file.clone(),
            line: edge.line,
            kind: edge.kind.clone(),
            name: edge.target.clone(),
            source: Some(edge.source.clone()),
        });
    let unresolved = oracle
        .unresolved
        .iter()
        .filter(|reference| !matcher.unresolved(reference))
        .map(|reference| FactKey {
            category: FactCategory::Unresolved,
            file: reference.file.clone(),
            line: reference.line,
            kind: reference.kind.clone(),
            name: reference.name.clone(),
            source: None,
        });
    symbols.chain(edges).chain(unresolved).collect()
}

/// Integrity findings for one mode's oracle document and corpus.
fn corpus_problems(case: &LanguageCase) -> Vec<String> {
    let oracle = &case.oracle;
    let mut problems = Vec::new();
    if oracle.baseline != V1_BASELINE || oracle.binary_sha256 != V1_BINARY_SHA256 {
        problems.push(format!(
            "captured from {} / {}, expected {V1_BASELINE} / {V1_BINARY_SHA256}",
            oracle.baseline, oracle.binary_sha256
        ));
    }
    if oracle.language != case.language.as_str() {
        problems.push(format!("oracle names language {}", oracle.language));
    }
    let corpus = case
        .corpus
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let listed = corpus
        .iter()
        .copied()
        .filter(|path| !path_leaf(path).starts_with('.'))
        .collect::<Vec<_>>();
    if listed != oracle.files {
        problems.push(format!(
            "corpus {listed:?} differs from the captured file list"
        ));
    }
    for file in fact_files(oracle) {
        if !corpus.contains(file) {
            problems.push(format!(
                "a v1 fact names {file}, which is not in the corpus"
            ));
        }
    }
    problems.extend(attribution_problems(case));
    problems.extend(unmapped_kind_problems(oracle));
    problems
}

/// Every file a v1 fact belongs to or points at.
fn fact_files(oracle: &Oracle) -> BTreeSet<&str> {
    let symbols = oracle.symbols.iter().map(|symbol| symbol.file.as_str());
    let edges = oracle
        .edges
        .iter()
        .flat_map(|edge| [edge.file.as_str(), edge.target_file.as_str()]);
    let unresolved = oracle
        .unresolved
        .iter()
        .map(|reference| reference.file.as_str());
    symbols.chain(edges).chain(unresolved).collect()
}

/// Attribution, structural-floor and binary-file findings for one mode.
fn attribution_problems(case: &LanguageCase) -> Vec<String> {
    let mut problems = Vec::new();
    let attributed = attributed_files(case);
    if attributed.is_empty() {
        problems.push("no corpus file is attributed to the mode".to_owned());
    }
    let structural = case
        .oracle
        .symbols
        .iter()
        .any(|symbol| symbol.kind != V1_FILE_KIND && attributed.contains(symbol.file.as_str()));
    let parser_only = PARSER_ONLY_V1_MODES.contains(&case.language);
    if structural == parser_only {
        problems.push(format!(
            "v1 structural facts in attributed files: {structural}; listed as parser-only: {parser_only}"
        ));
    }
    let binary = case
        .corpus
        .iter()
        .filter(|file| std::str::from_utf8(&file.bytes).is_err())
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    for symbol in &case.oracle.symbols {
        if symbol.kind != V1_FILE_KIND && binary.contains(symbol.file.as_str()) {
            problems.push(format!("binary file {} carries v1 facts", symbol.file));
        }
    }
    problems
}

/// v1 kinds the mapping tables do not cover.
fn unmapped_kind_problems(oracle: &Oracle) -> Vec<String> {
    let symbol_kinds = oracle
        .symbols
        .iter()
        .map(|symbol| symbol.kind.as_str())
        .filter(|kind| {
            ![V1_FILE_KIND, V1_IMPORT_KIND].contains(kind) && symbol_kind(kind).is_none()
        });
    let edge_kinds = oracle
        .edges
        .iter()
        .map(|edge| edge.kind.as_str())
        .chain(
            oracle
                .unresolved
                .iter()
                .map(|reference| reference.kind.as_str()),
        )
        .filter(|kind| *kind != V1_CONTAINS_KIND && reference_kinds(kind).is_none());
    symbol_kinds
        .chain(edge_kinds)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|kind| format!("v1 kind {kind} has no mapping"))
        .collect()
}

/// Compare every language's misses with the ledger.
fn reconcile(
    outcomes: &BTreeMap<String, LanguageOutcome>,
    ledger: &[LedgerEntry],
) -> BTreeMap<String, LanguageFailures> {
    let mut failures = BTreeMap::<String, LanguageFailures>::new();
    let ledgered = ledger
        .iter()
        .map(|entry| (entry.language.as_str(), &entry.fact))
        .collect::<BTreeSet<_>>();
    for (language, outcome) in outcomes {
        let failure = failures.entry(language.clone()).or_default();
        failure.unledgered = outcome
            .misses
            .iter()
            .filter(|fact| !ledgered.contains(&(language.as_str(), *fact)))
            .cloned()
            .collect();
        failure.problems.clone_from(&outcome.problems);
        failure.notes.clone_from(&outcome.notes);
    }
    for entry in ledger {
        let still_missing = outcomes
            .get(&entry.language)
            .is_some_and(|outcome| outcome.misses.contains(&entry.fact));
        if !still_missing {
            failures
                .entry(entry.language.clone())
                .or_default()
                .stale
                .push(entry.fact.clone());
        }
    }
    for failure in failures.values_mut() {
        failure.stale.sort();
    }
    failures
}

/// Write every unmatched fact to the path in [`REPORT_PATH_VARIABLE`], when set.
fn write_requested_report(outcomes: &BTreeMap<String, LanguageOutcome>) {
    let Some(path) = std::env::var_os(REPORT_PATH_VARIABLE) else {
        return;
    };
    let records = outcomes
        .iter()
        .flat_map(|(language, outcome)| {
            outcome.misses.iter().map(|fact| MissRecord {
                language: language.as_str(),
                fact,
            })
        })
        .collect::<Vec<_>>();
    let json = serde_json::to_string_pretty(&records)
        .unwrap_or_else(|error| panic!("cannot serialize the miss report: {error}"));
    fs::write(&path, json + "\n").unwrap_or_else(|error| {
        panic!(
            "cannot write the miss report to {}: {error}",
            Path::new(&path).display()
        )
    });
}

/// Render all failures, grouped by language with counts.
fn render_failures(failures: &BTreeMap<String, LanguageFailures>, fact_count: usize) -> String {
    let failing = failures
        .iter()
        .filter(|(_, failure)| !failure.is_clean())
        .collect::<Vec<_>>();
    let unledgered = failing
        .iter()
        .map(|(_, failure)| failure.unledgered.len())
        .sum::<usize>();
    let stale = failing
        .iter()
        .map(|(_, failure)| failure.stale.len())
        .sum::<usize>();
    let problems = failing
        .iter()
        .map(|(_, failure)| failure.problems.len())
        .sum::<usize>();
    let mut text = format!(
        "v1 parity oracle: {unledgered} unmatched v1 facts without a ledger entry, {stale} stale \
         ledger entries and {problems} problems in {} languages ({fact_count} v1 facts checked)\n",
        failing.len()
    );
    for (language, failure) in failing {
        render_language(&mut text, language, failure);
    }
    text
}

/// Render one language's failure group.
fn render_language(text: &mut String, language: &str, failure: &LanguageFailures) {
    let _ = writeln!(
        text,
        "== {language}: {} unmatched, {} stale, {} problems ==",
        failure.unledgered.len(),
        failure.stale.len(),
        failure.problems.len()
    );
    for fact in &failure.unledgered {
        let _ = writeln!(text, "  unmatched {}", describe(fact));
    }
    for fact in &failure.stale {
        let _ = writeln!(text, "  stale ledger entry {}", describe(fact));
    }
    for problem in &failure.problems {
        let _ = writeln!(text, "  problem: {problem}");
    }
    for note in &failure.notes {
        let _ = writeln!(text, "  note: {note}");
    }
}

/// One-line description of a fact.
fn describe(fact: &FactKey) -> String {
    let line = fact
        .line
        .map_or_else(|| "-".to_owned(), |line| line.to_string());
    let subject = fact.source.as_ref().map_or_else(
        || format!("`{}`", fact.name),
        |source| format!("`{source}` -> `{}`", fact.name),
    );
    let category = match fact.category {
        FactCategory::Symbol => "symbol",
        FactCategory::Edge => "edge",
        FactCategory::Unresolved => "unresolved",
    };
    format!("{category} {} {subject} at {}:{line}", fact.kind, fact.file)
}

/// v2 symbol kind for a v1 node kind.
fn symbol_kind(v1_kind: &str) -> Option<SymbolKind> {
    SYMBOL_KIND_MAP
        .iter()
        .find(|(kind, _)| *kind == v1_kind)
        .map(|(_, kind)| *kind)
}

/// v2 reference kinds for a v1 edge or reference kind.
fn reference_kinds(v1_kind: &str) -> Option<&'static [ReferenceKind]> {
    EDGE_KIND_MAP
        .iter()
        .find(|(kind, _)| *kind == v1_kind)
        .map(|(_, kinds)| *kinds)
}

/// Text after the last name separator; the whole name when there is none.
fn final_segment(name: &str) -> &str {
    NAME_SEPARATORS
        .iter()
        .filter_map(|separator| name.rfind(separator).map(|index| index + separator.len()))
        .max()
        .map_or(name, |start| &name[start..])
}

/// Text before the first name separator; the whole name when there is none.
fn leading_segment(name: &str) -> &str {
    NAME_SEPARATORS
        .iter()
        .filter_map(|separator| name.find(separator))
        .min()
        .map_or(name, |end| &name[..end])
}

/// The first name separator, in [`NAME_SEPARATORS`] order, that occurs in `name`.
fn primary_separator(name: &str) -> Option<&'static str> {
    NAME_SEPARATORS
        .into_iter()
        .find(|separator| name.contains(separator))
}

/// Whether a v2 name agrees with a v1 name: equal, or sharing a non-empty final segment.
fn names_agree(v2_name: &str, v1_name: &str) -> bool {
    let segment = final_segment(v1_name);
    v2_name == v1_name || !segment.is_empty() && final_segment(v2_name) == segment
}

/// Whether a v2 qualified name agrees with a v1 node's qualified name under the
/// symbol rule. A v1 name scoped by the node's own file path is a synthetic
/// framework identity, so only the node's kind, name and line identify it.
fn qualified_names_agree(symbol: &V1Symbol, v2_name: &str) -> bool {
    let v1_name = symbol.qualified_name.as_str();
    if is_file_scoped_identity(v1_name, &symbol.file) {
        return true;
    }
    match (primary_separator(v1_name), primary_separator(v2_name)) {
        (Some(left), Some(right)) if left == right => {
            v2_name == v1_name
                || v2_name.strip_suffix(v1_name).is_some_and(|prefix| {
                    NAME_SEPARATORS
                        .iter()
                        .any(|separator| prefix.ends_with(separator))
                })
        }
        _ => final_segment(v1_name) == final_segment(v2_name),
    }
}

/// Whether a qualified name starts with a `<file>::` or `<file>#` scope.
fn is_file_scoped_identity(qualified_name: &str, file: &str) -> bool {
    qualified_name.strip_prefix(file).is_some_and(|rest| {
        FILE_SCOPE_SEPARATORS
            .iter()
            .any(|separator| rest.starts_with(separator))
    })
}

/// Last path component of a module specifier or file path.
fn path_leaf(path: &str) -> &str {
    path.rsplit(PATH_SEPARATORS).next().unwrap_or(path)
}

/// A path component without its final extension, unless that leaves nothing.
fn path_stem(component: &str) -> &str {
    match component.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => component,
    }
}

/// Whether an import specifier names a non-import v1 target.
fn specifier_names_target(specifier: &str, target: &V1Symbol) -> bool {
    let leaf = path_leaf(specifier);
    let identities = [target.name.as_str(), path_stem(&target.name)];
    [specifier, leaf, path_stem(leaf), final_segment(specifier)]
        .into_iter()
        .any(|candidate| !candidate.is_empty() && identities.contains(&candidate))
}
