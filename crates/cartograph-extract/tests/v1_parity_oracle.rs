//! Exact v1.1.33 parity for native, per-file extraction.
//!
//! Each of the 73 v1 language modes owns a frozen corpus under
//! `tests/fixtures/v1_parity/<language>/`. Its `expected/<language>.json` contains
//! files, declarations, edges and unresolved references captured once from the
//! real v1.1.33 binary. The binary hash, corpus membership and 8,231 raw records
//! are checked here; captured facts and corpus source are never regenerated to
//! make a v2 extraction pass. Duplicate captures share one disposition, giving
//! 8,165 distinct fact selectors.
//!
//! # Three dispositions
//!
//! **Identity** requires one exact native observation. **Alignment** requires
//! every explicitly committed native pin for a genuine shape difference or
//! documented correction. **Ledger** records a missing fact as pending with a
//! wave and gap ID, or intentional with a reason ID. The reason table stores
//! shared evidence once. Nearest-candidate suggestions are diagnostics only.
//!
//! Keys are arrays of readable strings, scoped by language and corpus-relative
//! file. `F` names a document root. `S` names a symbol by kind, full qualified
//! name, start line and display name; identity omits the display component from
//! its index alias but still checks it. `C` names containment by parent kind/name,
//! child kind/name and the child's start line. A line-less capture uses the
//! five-component alias; a recorded line remains part of identity. `R` names a
//! reference by reference kind, owner kind, full owner qualified name, owner
//! declaration start, exact reference name and occurrence start. A line-less
//! capture may omit only the occurrence line. File owners use `file`, the path
//! and declaration line 1. `B` names an import binding by binding kind, module,
//! imported/local names and start line; `T` names inline-test presence. `U` retains
//! an unresolved capture selector for an explicit disposition.
//!
//! The index includes ALL native facts in each file, including duplicate keys.
//! An identity key selecting several observations is ambiguous and never carries
//! a fact. An explicit end line may distinguish a canonical pin; if the complete
//! readable selector still selects several observations, document the ambiguity
//! rather than choosing one. Byte offsets and columns never participate.
//!
//! A pin is a canonical native key with an optional end line. Canonical keys keep
//! native lookup names, file-owner markers and any necessary parent-site
//! discriminators. An alignment cannot use a shorter identity alias. Multiple
//! pins are conjunctive: all must select distinct observations exactly once.
//! Verified symbol correspondences supply exact endpoints for relationship
//! identity, preserving the native owner or child declaration rather than merely
//! agreeing on a leaf name.
//!
//! # Committed JSONL tables
//!
//! All five tables live directly under `tests/fixtures/v1_parity/`. Each object
//! is one row; the examples below are wrapped only for display. Alignment,
//! divergence and reason tables are sorted by their typed record ordering.
//!
//! `alignments.jsonl`: a frozen selector and the complete group of native pins;
//! `end` disambiguates the frozen selector and `reason` annotates corrections.
//! ```json
//! {"language":"vue","file":"App.vue","v1":["F","8"],
//!  "pins":[{"key":["F","1"]}],"reason":"file-document-scope"}
//! ```
//! `divergences.jsonl`: a frozen fact and a pending or intentional disposition.
//! ```json
//! {"language":"c","file":"src/video/video.c",
//!  "fact":{"key":["S","variable","AX_S32","7","AX_S32"]},
//!  "disposition":{"status":"intentional","id":"c-macro-return-type-as-variable"}}
//! ```
//! `reasons.jsonl`: one nonempty reason per ID, with a wave only for pending gaps.
//! ```json
//! {"id":"codeigniter-route-normalization","wave":3,
//!  "reason":"$route['404_override'] is v1 ANY <404>; native names ANY /404_override."}
//! ```
//! `gate_cases.jsonl`: small in-memory inputs and expected diagnostic fragments.
//! ```json
//! {"name":"missing symbol","cases":[{"language":"cpp",
//!  "captured":[{"file":"a.cpp","pin":{"key":["S","class","A","1","A"]}}],
//!  "native":[]}],"policy":{"rows":[],"definitions":[]},"problems":["unmatched"]}
//! ```
//! `counterexamples.jsonl`: review regressions, usually with real corpus input
//! and edits to the native extraction before checking the same gate.
//! ```json
//! {"name":"changed symbol kind","cases":[{"language":"cpp",
//!  "captured":[{"file":"a.cpp","pin":{"key":["S","class","A","1","A"]}}],
//!  "native":[]}],"policy":{"rows":[],"definitions":[]},"problems":["unmatched"],
//!  "input":{"path":"a.cpp","source":"class A {};",
//!  "mutations":[{"collection":"symbols","index":0,"field":"kind","value":"variable"}]}}
//! ```
//!
//! # Resolving a failure
//!
//! Run `cargo test --locked -p cartograph-extract --test v1_parity_oracle --
//! --nocapture`. Read the frozen fact and up to three same-file suggestions near
//! its line or leaf name. Inspect the frozen source, capture and actual native
//! extraction to establish correspondence; a suggestion alone proves nothing.
//! Fix an extractor regression where appropriate. For a genuine shape difference,
//! commit exact pins (the entire established group for a split representation),
//! using end lines only when needed for uniqueness. For a fact not carried, add
//! a pending gap or an intentional reason with concrete source evidence. Reuse
//! reason IDs, preserve existing pending waves, sort the policy tables and rerun
//! the gate and regressions. Never widen matching rules to silence a failure.
//!
//! A row referencing an absent frozen selector, two dispositions for one selector,
//! undefined or unused reason IDs, a wrong wave, empty/duplicate/overlapping pins
//! or a now-successful identity match fails hygiene. Remove a stale alignment or
//! ledger row and remove its reason definition if no other row uses that ID.
//!
//! # Corrected target occurrences
//!
//! Alignments annotated with `v1-target-misresolution-*` share an occurrence-only
//! contract: they require the corrected source occurrence under its exact typed
//! owner, owner declaration site and occurrence site. They explicitly reject the
//! captured resolved target and NEVER claim resolved-target parity. Each reason
//! supplies entity-specific source evidence. Correct target resolution requires
//! separate resolver evidence; this per-file extraction gate does not establish
//! it, even when the native reference name expresses the intended receiver.

mod dependency_ownership;

use cartograph_domain::{SourceLanguage, SymbolId, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};
use serde::{Deserialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::{fmt::Write, fs, path::Path, sync::OnceLock};

use Disposition::{Aligned, Intentional, Pending};

/// Root of the immutable corpora, captures and committed policy tables.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1_parity");

/// Release identifier sealed into every immutable capture.
const BASELINE: &str = "v1.1.33";

/// SHA-256 of the v1.1.33 binary that produced all captures.
const BINARY_SHA256: &str = "c223ca62ad1a31f2686c7e79f7175fde440e7f98135062eb3ed8f09937b7e5c7";

/// Number of legacy language modes required by this gate.
const LANGUAGE_COUNT: usize = 73;

/// Raw frozen record count, including repeated captures.
const FACT_COUNT: usize = 8_231;

/// Declaration line of a native file root.
const DOCUMENT_START: u32 = 1;

/// Maximum size of one corpus input passed to the native extractor.
const SOURCE_BYTES: usize = 1_024 * 1_024;

/// Maximum diagnostic candidates for an unmatched fact.
const SUGGESTIONS: usize = 3;

/// Allowed pending waves; table definitions must agree with their rows.
const WAVES: [u8; 3] = [1, 2, 3];

/// Immutable records produced by the released v1.1.33 capture binary.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Oracle {
    baseline: String,
    binary_sha256: String,
    language: String,
    files: Vec<String>,
    symbols: Vec<V1Symbol>,
    edges: Vec<V1Edge>,
    unresolved: Vec<V1Unresolved>,
}

/// A frozen declaration, including its display name and recorded line range.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Symbol {
    file: String,
    kind: String,
    name: String,
    qualified_name: String,
    start_line: u32,
    end_line: u32,
}

/// A frozen relationship whose optional line must never be silently discarded.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Edge {
    file: String,
    kind: String,
    source_kind: String,
    source: String,
    target_kind: String,
    target: String,
    target_file: String,
    line: Option<u32>,
}

/// A frozen unresolved reference requiring an explicit correspondence or ledger row.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Unresolved {
    file: String,
    kind: String,
    name: String,
    line: Option<u32>,
}

/// A projected observation and its exact relationship endpoints, when recorded.
#[derive(Clone, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct Fact {
    file: String,
    /// Frozen selector for captures; canonical key for native observations.
    pin: Pin,
    /// Capture identity query before verified declaration endpoints are substituted.
    query: Option<Pin>,
    /// Captured kind/name pair or complete native declaration identity.
    owner: Option<Vec<String>>,
    /// Captured kind/name pair or complete native containment-child identity.
    target: Option<Vec<String>>,
}

/// One language's frozen facts and current native per-file observations.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    language: String,
    captured: Vec<Fact>,
    native: Vec<Fact>,
}

/// A ledger record, also used internally after loading an alignment record.
#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct Row {
    language: String,
    file: String,
    fact: Pin,
    disposition: Disposition,
}

/// A flat alignment row: one frozen selector and a mandatory group of native pins.
#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct Alignment {
    language: String,
    file: String,
    v1: Vec<String>,
    end: Option<u32>,
    pins: Vec<Pin>,
    reason: Option<String>,
}

/// Shared evidence for a gap or intentional/correction reason.
#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct Definition {
    id: String,
    reason: String,
    wave: Option<u8>,
}

/// Explicit dispositions and their shared reason definitions.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    rows: Vec<Row>,
    definitions: Vec<Definition>,
}

/// A small gate case or a mutation of real native extraction output.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Example {
    name: String,
    cases: Vec<Case>,
    policy: Policy,
    problems: Vec<String>,
    input: Option<NativeInput>,
}

/// Source and native-output edits used to replay an adversarial counterexample.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeInput {
    path: String,
    source: Option<String>,
    mutations: Vec<Mutation>,
}

/// One deletion or field edit, optionally selecting an owner by its symbol key.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    collection: String,
    index: usize,
    field: Option<String>,
    value: serde_json::Value,
    symbol: Option<Vec<String>>,
}

/// An exact readable key, optionally narrowed by a declaration/occurrence end line.
#[derive(Clone, Deserialize, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct Pin {
    key: Vec<String>,
    end: Option<u32>,
}

/// Explicit policy for a fact that lacks a unique identity correspondence.
#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields, tag = "status", rename_all = "snake_case")]
enum Disposition {
    Aligned {
        v2: Vec<Pin>,
        reason: Option<String>,
    },
    Pending {
        wave: u8,
        id: String,
    },
    Intentional {
        id: String,
    },
}

/// Owned identity of a frozen fact within its language and file.
type Id = (String, String, Pin);

/// Disposition label, selected observation indices and diagnostic problems.
type Decision = (String, Vec<usize>, Vec<String>);

/// Gate diagnostics, unique-disposition counts and carried observations.
#[derive(Default)]
struct Outcome {
    problems: Vec<String>,
    counts: BTreeMap<String, usize>,
    carried: BTreeMap<Id, (Vec<usize>, bool)>,
}

/// Surface fixture and extraction errors with their original message.
fn must<Value, Error: std::fmt::Display>(value: Result<Value, Error>) -> Value {
    value.unwrap_or_else(|error| panic!("{error}"))
}

/// Build readable string components without escaping or nested serialized keys.
macro_rules! key {
    ($($part:expr),* $(,)?) => { vec![$($part.to_string()),*] };
}

/// Construct an observation with a readable key and no relationship metadata.
fn fact(file: &str, parts: Vec<String>) -> Fact {
    Fact {
        file: file.into(),
        pin: Pin {
            key: parts,
            end: None,
        },
        ..Fact::default()
    }
}

/// Keep the frozen symbol selector, including its display name and declaration site.
fn node_key(symbol: &V1Symbol) -> Vec<String> {
    match symbol.kind.as_str() {
        "file" => key!["F", symbol.start_line],
        _ => key![
            "S",
            &symbol.kind,
            &symbol.qualified_name,
            symbol.start_line,
            &symbol.name
        ],
    }
}

/// Project immutable captures without inventing missing sites or relationships.
fn captured(oracle: &Oracle) -> Vec<Fact> {
    let mut facts = Vec::new();
    for symbol in &oracle.symbols {
        assert!(symbol.start_line > 0 && symbol.end_line >= symbol.start_line);
        if symbol.kind != "file" {
            parse::<SymbolKind>(&format!("\"{}\"", symbol.kind));
        }
        let mut captured_fact = fact(&symbol.file, node_key(symbol));
        captured_fact.query = Some(Pin {
            key: captured_fact.pin.key.clone(),
            end: Some(symbol.end_line),
        });
        if oracle.symbols.iter().any(|other| {
            other.file == symbol.file
                && node_key(other) == captured_fact.pin.key
                && other.end_line != symbol.end_line
        }) {
            captured_fact.pin.end = Some(symbol.end_line);
        }
        facts.push(captured_fact);
    }
    facts.extend(
        oracle
            .edges
            .iter()
            .map(|edge| captured_edge(edge, &oracle.symbols)),
    );
    for unresolved in &oracle.unresolved {
        let mut captured_fact = fact(
            &unresolved.file,
            key!["U", &unresolved.kind, &unresolved.name],
        );
        if let Some(line) = unresolved.line {
            captured_fact.pin.key.push(line.to_string());
        }
        facts.push(captured_fact);
    }
    facts
}

/// Retain the frozen selector and derive a typed owner query from captured declarations.
fn captured_edge(edge: &V1Edge, symbols: &[V1Symbol]) -> Fact {
    let contains = edge.kind == "contains";
    let key = if contains {
        key![
            "C",
            &edge.source_kind,
            &edge.source,
            &edge.target_kind,
            &edge.target
        ]
    } else {
        key!["R", &edge.kind, &edge.source, &edge.target]
    };
    let mut captured_fact = fact(&edge.file, key);
    if let Some(line) = edge.line {
        captured_fact.pin.key.push(line.to_string());
    }
    captured_fact.query = Some(captured_fact.pin.clone());
    if !contains {
        let owners = symbols
            .iter()
            .filter(|symbol| {
                symbol.file == edge.file
                    && symbol.kind == edge.source_kind
                    && symbol.qualified_name == edge.source
            })
            .filter(|symbol| {
                edge.line
                    .is_none_or(|line| (symbol.start_line..=symbol.end_line).contains(&line))
            })
            .map(|symbol| symbol.start_line)
            .collect::<BTreeSet<_>>();
        let site = if edge.source_kind == "file" {
            DOCUMENT_START
        } else if owners.len() == 1 {
            owners.first().copied().unwrap_or_default()
        } else {
            0
        };
        let (source_kind, target_kind) = (&edge.source_kind, &edge.target_kind);
        let mut query = key![
            "R",
            &edge.kind,
            source_kind,
            &edge.source,
            site,
            &edge.target
        ];
        if let Some(line) = edge.line {
            query.push(line.to_string());
        }
        captured_fact.query = Some(Pin {
            key: query,
            end: None,
        });
        captured_fact
            .pin
            .key
            .extend(key!["owner_kind", source_kind, "target_kind", target_kind]);
    }
    if edge.target_file != edge.file {
        captured_fact
            .pin
            .key
            .extend(key!["target_file", &edge.target_file]);
    }
    captured_fact.owner = Some(key![&edge.source_kind, &edge.source]);
    captured_fact.target = Some(key![&edge.target_kind, &edge.target]);
    captured_fact
}

/// Name a native declaration by exact kind, qualified name, start and display name.
fn symbol_key(symbol: &ExtractedSymbol) -> Vec<String> {
    let kind = symbol.kind.as_str();
    let qualified_name = &symbol.qualified_name;
    let line = symbol.span.start_line();
    key!["S", kind, qualified_name, line, symbol.name]
}

/// Include the end line when comparing a relationship to a verified declaration.
fn concrete(pin: &Pin) -> Vec<String> {
    let mut key = pin.key.clone();
    key.push(pin.end.unwrap_or_default().to_string());
    key
}

/// Attach complete native owner identity and readable collision discriminators.
fn owner(native_fact: &mut Fact, symbol: &ExtractedSymbol, symbols: &[ExtractedSymbol]) {
    let mut identity = symbol_key(symbol);
    identity.push(symbol.span.end_line().to_string());
    native_fact.owner = Some(identity);
    let peers = symbols
        .iter()
        .filter(|peer| peer.qualified_name == symbol.qualified_name);
    let (kind, line) = (symbol.kind.as_str(), symbol.span.start_line());
    let key = &mut native_fact.pin.key;
    if key[0] == "C" && peers.clone().count() > 1 {
        key.extend(key!["parent_kind", kind, "parent_line", line]);
    }
    // A shared name/kind/start needs the owner's end line even for references.
    let site = (symbol.kind, symbol.span.start_line());
    if peers
        .filter(|peer| (peer.kind, peer.span.start_line()) == site)
        .count()
        > 1
    {
        key.extend(key!["parent_end", symbol.span.end_line()]);
    }
}

/// Project every native observation in stable order; never coalesce duplicate keys.
fn native(file: &ExtractedFile) -> Vec<Fact> {
    let path = file.path.as_str();
    let symbols_by_id: BTreeMap<_, _> = file
        .symbols
        .iter()
        .map(|symbol| (&symbol.id, symbol))
        .collect();
    assert_eq!(
        symbols_by_id.len(),
        file.symbols.len(),
        "duplicate native symbol id"
    );
    let mut facts = vec![fact(path, key!["F", DOCUMENT_START])];
    for symbol in &file.symbols {
        let mut native_fact = fact(path, symbol_key(symbol));
        native_fact.pin.end = Some(symbol.span.end_line());
        facts.push(native_fact);
    }
    append_containments(file, &symbols_by_id, &mut facts);
    append_references(file, &symbols_by_id, &mut facts);
    append_bindings(file, &mut facts);
    if file.has_inline_tests {
        facts.push(fact(path, key!["T",]));
    }
    facts
}

/// Append real containment observations followed by implicit file-root relationships.
fn append_containments(
    file: &ExtractedFile,
    symbols_by_id: &BTreeMap<&SymbolId, &ExtractedSymbol>,
    facts: &mut Vec<Fact>,
) {
    let path = file.path.as_str();
    let children = file
        .containments
        .iter()
        .map(|containment| &containment.child)
        .collect::<BTreeSet<_>>();
    let roots = file
        .symbols
        .iter()
        .filter(|symbol| !children.contains(&symbol.id))
        .map(|symbol| (None, symbol));
    let edges = file.containments.iter().map(|containment| {
        (
            Some(symbols_by_id[&containment.parent]),
            symbols_by_id[&containment.child],
        )
    });
    for (parent, child) in edges.chain(roots) {
        let (kind, qualified_name) = parent.map_or(("file", path), |parent_symbol| {
            (parent_symbol.kind.as_str(), &*parent_symbol.qualified_name)
        });
        let (child_kind, child_name, line) = (
            child.kind.as_str(),
            &child.qualified_name,
            child.span.start_line(),
        );
        let mut native_fact = fact(
            path,
            key!["C", kind, qualified_name, child_kind, child_name, line],
        );
        native_fact.pin.end = Some(child.span.end_line());
        native_fact.target = Some(concrete(&Pin {
            key: symbol_key(child),
            end: native_fact.pin.end,
        }));
        if let Some(parent) = parent {
            owner(&mut native_fact, parent, &file.symbols);
        } else {
            native_fact.pin.key.push("file_owner".into());
        }
        facts.push(native_fact);
    }
}

/// Append references with typed owners and every canonical lookup discriminator.
fn append_references(
    file: &ExtractedFile,
    symbols_by_id: &BTreeMap<&SymbolId, &ExtractedSymbol>,
    facts: &mut Vec<Fact>,
) {
    let path = file.path.as_str();
    for reference in &file.references {
        let parent = reference.owner.as_ref().map(|id| symbols_by_id[id]);
        let (parent_kind, parent_name, parent_line) =
            parent.map_or(("file", path, DOCUMENT_START), |parent_symbol| {
                (
                    parent_symbol.kind.as_str(),
                    parent_symbol.qualified_name.as_str(),
                    parent_symbol.span.start_line(),
                )
            });
        let (kind, name, line) = (
            reference.kind.as_str(),
            &reference.name,
            reference.span.start_line(),
        );
        let mut native_fact = fact(
            path,
            key!["R", kind, parent_kind, parent_name, parent_line, name, line],
        );
        if let Some(lookup) = &reference.resolution_name {
            native_fact.pin.key.extend(key!["lookup", lookup]);
        }
        native_fact.pin.end = Some(reference.span.end_line());
        if let Some(parent) = parent {
            owner(&mut native_fact, parent, &file.symbols);
        } else {
            native_fact.pin.key.push("file_owner".into());
        }
        facts.push(native_fact);
    }
}

/// Append import bindings as a separate native observation category.
fn append_bindings(file: &ExtractedFile, facts: &mut Vec<Fact>) {
    let path = file.path.as_str();
    for binding in &file.import_bindings {
        let kind = must(serde_json::to_string(&binding.kind));
        let kind = kind.trim_matches('"');
        let module = &binding.module_specifier;
        let imported = &binding.imported_name;
        let local = &binding.local_name;
        let line = binding.span.start_line();
        let mut native_fact = fact(path, key!["B", kind, module, imported, local, line]);
        native_fact.pin.end = Some(binding.span.end_line());
        facts.push(native_fact);
    }
}

/// Canonical and identity-alias keys mapped to every native observation index.
type Index<'a> = BTreeMap<(&'a str, Vec<String>), Vec<usize>>;

/// Index canonical keys and identity aliases over all native observations.
fn index(facts: &[Fact]) -> Index<'_> {
    let mut index = Index::new();
    for (observation_index, native_fact) in facts.iter().enumerate() {
        // Aliases simplify identity only; each distinct observation remains indexed.
        let mut keys = vec![native_fact.pin.key.clone()];
        let lengths: &[usize] = match native_fact.pin.key[0].as_str() {
            "S" => &[4],
            "C" => &[5, 6],
            "R" => &[6, 7],
            _ => &[],
        };
        keys.extend(
            lengths
                .iter()
                .map(|length| native_fact.pin.key[..*length].to_vec()),
        );
        for key in keys.into_iter().collect::<BTreeSet<_>>() {
            index
                .entry((native_fact.file.as_str(), key))
                .or_default()
                .push(observation_index);
        }
    }
    index
}

/// Read a strictly typed capture or policy record.
fn parse<Record: DeserializeOwned>(text: &str) -> Record {
    must(serde_json::from_str(text))
}

/// Read fixture text while preserving a useful filesystem error.
fn read(path: &Path) -> String {
    must(fs::read_to_string(path))
}

/// Parse one strict record per line from a fixture table.
fn jsonl<Record: DeserializeOwned>(file: &str) -> Vec<Record> {
    let text = read(&Path::new(ROOT).join(file));
    text.lines().map(parse).collect()
}

/// List every corpus file deterministically, including hidden extraction inputs.
fn corpus_files(root: &Path) -> Vec<String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in must(fs::read_dir(directory)) {
            let path = must(entry).path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(
                    must(path.strip_prefix(root))
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    files.sort();
    files
}

/// Run the production per-file extractor with the corpus-relative virtual path.
fn extract(path: &str, bytes: &[u8]) -> Result<ExtractedFile, String> {
    let limits = SourceLimits::new(SOURCE_BYTES).map_err(|error| error.to_string())?;
    let snapshot =
        SourceSnapshot::from_bytes(path, bytes, limits).map_err(|error| error.to_string())?;
    NativeExtractor::new(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .map_err(|error| error.to_string())
}

/// Verify capture provenance and corpus membership before extracting a language corpus.
fn load_case(language: SourceLanguage) -> Case {
    let language_name = language.as_str();
    let root = Path::new(ROOT).join(language_name);
    let expected = Path::new(ROOT).join(format!("expected/{language_name}.json"));
    let oracle: Oracle = parse(&read(&expected));
    assert_eq!(oracle.baseline, BASELINE);
    assert_eq!(oracle.binary_sha256, BINARY_SHA256);
    assert_eq!(oracle.language, language_name);
    let files = corpus_files(&root);
    let visible = files
        .iter()
        .filter(|path| !path.rsplit('/').next().unwrap_or_default().starts_with('.'))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(visible, oracle.files, "{language_name} corpus changed");
    let fact_files = oracle
        .symbols
        .iter()
        .map(|symbol| &symbol.file)
        .chain(
            oracle
                .edges
                .iter()
                .flat_map(|edge| [&edge.file, &edge.target_file]),
        )
        .chain(oracle.unresolved.iter().map(|unresolved| &unresolved.file));
    assert!(
        fact_files.into_iter().all(|file| files.contains(file)),
        "captured file missing"
    );
    let mut native_facts = Vec::new();
    for path in files {
        let extraction = extract(&path, &must(fs::read(root.join(&path))));
        if oracle
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "file" && symbol.file == path)
        {
            assert!(extraction.is_ok(), "{language_name} {path}: {extraction:?}");
        }
        if let Ok(file) = extraction {
            assert_eq!(file.path.as_str(), path);
            native_facts.extend(native(&file));
        }
    }
    Case {
        captured: captured(&oracle),
        language: oracle.language,
        native: native_facts,
    }
}

/// Load the immutable captures and committed policy once for all test entry points.
fn corpus() -> &'static (Vec<Case>, Policy) {
    static CORPUS: OnceLock<(Vec<Case>, Policy)> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let languages = SourceLanguage::ALL
            .into_iter()
            .filter(|language| language.is_v1_language())
            .collect::<Vec<_>>();
        let expected = corpus_files(&Path::new(ROOT).join("expected")).len();
        let cases = languages.into_iter().map(load_case).collect::<Vec<_>>();
        let facts = cases.iter().map(|case| case.captured.len()).sum::<usize>();
        assert_eq!(
            (cases.len(), expected, facts),
            (LANGUAGE_COUNT, LANGUAGE_COUNT, FACT_COUNT)
        );
        let mut rows = jsonl::<Alignment>("alignments.jsonl")
            .into_iter()
            .map(|alignment| Row {
                language: alignment.language,
                file: alignment.file,
                fact: Pin {
                    key: alignment.v1,
                    end: alignment.end,
                },
                disposition: Aligned {
                    v2: alignment.pins,
                    reason: alignment.reason,
                },
            })
            .collect::<Vec<_>>();
        rows.extend(jsonl::<Row>("divergences.jsonl"));
        (
            cases,
            Policy {
                rows,
                definitions: jsonl("reasons.jsonl"),
            },
        )
    })
}

/// Return the shared gap or reason identifier, including annotated corrections.
fn disposition_id(disposition: &Disposition) -> Option<&str> {
    match disposition {
        Aligned { reason, .. } => reason.as_deref(),
        Pending { id, .. } | Intentional { id } => Some(id),
    }
}

/// Extract a leaf and optional occurrence line for diagnostics only.
fn location(observation: &Fact) -> (&str, Option<u32>) {
    let key = &observation.query.as_ref().unwrap_or(&observation.pin).key;
    let (name_index, line_index) = match observation.pin.key[0].as_str() {
        "S" | "U" => (2, 3),
        "C" => (4, 5),
        "R" => (5, 6),
        "B" => (3, 5),
        _ => (0, 0),
    };
    let mut leaf = key[name_index].rsplit([':', '.', '\\', '/']);
    (
        leaf.next().unwrap_or_default(),
        key.get(line_index)
            .and_then(|line_text| line_text.parse().ok()),
    )
}

/// Show nearby native observations without granting them matching authority.
fn suggestions(wanted: &Fact, candidates: &[Fact]) -> String {
    let (name, line) = location(wanted);
    let mut near = candidates
        .iter()
        .filter(|candidate| {
            let (candidate_name, candidate_line) = location(candidate);
            candidate.file == wanted.file
                && (line.is_some() && line == candidate_line || candidate_name == name)
        })
        .collect::<Vec<_>>();
    near.sort_by_key(|candidate| (location(candidate).1 != line, &candidate.pin));
    let mut output = String::new();
    for candidate in near.into_iter().take(SUGGESTIONS) {
        must(write!(output, "\n  suggestion: {:?}", candidate.pin));
    }
    output
}

/// Borrowed identity used when looking up policy for a frozen fact.
type IdRef<'a> = (&'a str, &'a str, &'a Pin);

/// One explicit disposition per captured selector.
type Rows<'a> = BTreeMap<IdRef<'a>, &'a Disposition>;

/// Address a frozen fact within its language and file.
fn fact_id<'a>(language: &'a str, captured_fact: &'a Fact) -> IdRef<'a> {
    (language, &captured_fact.file, &captured_fact.pin)
}

/// Shared reason definitions indexed by their unique identifier.
type Definitions<'a> = BTreeMap<&'a str, &'a Definition>;

/// Validate the shared reason table and index its unique identifiers.
fn definitions<'a>(policy: &'a Policy, outcome: &mut Outcome) -> Definitions<'a> {
    let mut definitions = BTreeMap::new();
    for definition in &policy.definitions {
        if definition.id.trim().is_empty()
            || definition.reason.trim().is_empty()
            || definition.wave.is_some_and(|wave| !WAVES.contains(&wave))
            || definitions
                .insert(definition.id.as_str(), definition)
                .is_some()
        {
            outcome
                .problems
                .push(format!("invalid/duplicate definition {}", definition.id));
        }
    }
    definitions
}

/// Require defined identifiers with the right wave and nonempty, distinct pins.
fn validate_row(row: &Row, definitions: &Definitions<'_>, outcome: &mut Outcome) {
    if let Some(id) = disposition_id(&row.disposition) {
        let wave = match row.disposition {
            Pending { wave, .. } => Some(wave),
            _ => None,
        };
        if definitions
            .get(id)
            .is_none_or(|definition| definition.wave != wave)
        {
            outcome
                .problems
                .push(format!("undefined or wrong-wave id {id}"));
        }
    }
    if let Aligned { v2, .. } = &row.disposition
        && (v2.is_empty() || v2.iter().collect::<BTreeSet<_>>().len() != v2.len())
    {
        outcome
            .problems
            .push(format!("empty/duplicate pins for {:?}", row.fact));
    }
}

/// Reject unknown facts, duplicate dispositions and unused reason definitions.
fn policy_rows<'a>(cases: &[Case], policy: &'a Policy, outcome: &mut Outcome) -> Rows<'a> {
    let definitions = definitions(policy, outcome);
    let captured = cases
        .iter()
        .flat_map(|case| {
            case.captured
                .iter()
                .map(|captured_fact| fact_id(&case.language, captured_fact))
        })
        .collect::<BTreeSet<_>>();
    let mut rows = BTreeMap::new();
    let mut used = BTreeSet::new();
    for row in &policy.rows {
        let id = (row.language.as_str(), row.file.as_str(), &row.fact);
        if !captured.contains(&id) {
            outcome.problems.push(format!("unknown v1 fact {id:?}"));
        }
        if rows.insert(id, &row.disposition).is_some() {
            outcome
                .problems
                .push(format!("two dispositions for {id:?}"));
        }
        if let Some(id) = disposition_id(&row.disposition) {
            used.insert(id);
        }
        validate_row(row, &definitions, outcome);
    }
    for id in definitions.keys().filter(|id| !used.contains(**id)) {
        outcome.problems.push(format!("unused definition {id}"));
    }
    rows
}

/// Use a recorded occurrence to distinguish captured declaration sites.
fn at_node(node: &Fact, child: bool, line: u32) -> bool {
    let start = must(node.pin.key[3].parse());
    let end = node
        .query
        .as_ref()
        .and_then(|query| query.end)
        .or(node.pin.end);
    let end = if child { start } else { end.unwrap_or(start) };
    (start..=end).contains(&line)
}

/// Per-case identity index and previously verified declaration correspondences.
struct Gate<'a> {
    case: &'a Case,
    index: Index<'a>,
    nodes: BTreeMap<(String, Pin), usize>,
}

impl Gate<'_> {
    /// Select canonical pins only; identity aliases cannot satisfy an alignment.
    fn matches(&self, file: &str, pin: &Pin) -> Vec<usize> {
        self.candidates(file, pin)
            .into_iter()
            .filter(|observation_index| self.case.native[*observation_index].pin.key == pin.key)
            .collect()
    }

    /// Select indexed observations, applying an explicit end-line discriminator.
    fn candidates(&self, file: &str, pin: &Pin) -> Vec<usize> {
        self.index
            .get(&(file, pin.key.clone()))
            .into_iter()
            .flatten()
            .copied()
            .filter(|observation_index| {
                pin.end
                    .is_none_or(|end| self.case.native[*observation_index].pin.end == Some(end))
            })
            .collect()
    }

    /// Use readable identity without spans, preserving recorded containment lines.
    fn base_identity(&self, file: &str, pin: &Pin) -> Vec<usize> {
        let symbol = pin.key[0] == "S";
        let mut base = pin.clone();
        base.end = None;
        if symbol {
            // Display names still have to agree after the unique declaration lookup.
            base.key.truncate(4);
        } else if pin.key[0] == "C" {
            // A captured containment line is evidence, not an optional span detail.
            let length = if pin
                .key
                .get(5)
                .is_some_and(|component| component.parse::<u32>().is_ok())
            {
                6
            } else {
                5
            };
            base.key.truncate(length);
        }
        let found = self.candidates(file, &base);
        if let [observation_index] = found.as_slice() {
            let key = &self.case.native[*observation_index].pin.key;
            if symbol && key[4] != pin.key[4]
                || pin.key[0] == "R" && key.get(7).is_some_and(|component| component == "lookup")
            {
                return Vec::new();
            }
        }
        found
    }

    /// Reuse a uniquely captured declaration only after its correspondence is verified.
    fn endpoint(&self, captured_fact: &Fact, child: bool) -> Option<&Fact> {
        let endpoint =
            [&captured_fact.owner, &captured_fact.target][usize::from(child)].as_ref()?;
        let mut nodes = self
            .case
            .captured
            .iter()
            .filter(|symbol| {
                symbol.file == captured_fact.file
                    && symbol.pin.key[0] == "S"
                    && symbol.pin.key[1..3] == endpoint[..]
            })
            .map(|symbol| (&symbol.pin, symbol))
            .collect::<BTreeMap<_, _>>();
        if nodes.len() > 1 {
            let query = captured_fact.query.as_ref()?;
            let position = if query.key[0] == "C" { 5 } else { 6 };
            let line = query
                .key
                .get(position)
                .and_then(|line_text| line_text.parse::<u32>().ok());
            if let Some(line) = line {
                nodes.retain(|_, symbol| at_node(symbol, child, line));
            }
        }
        let node = nodes.values().next().filter(|_| nodes.len() == 1)?;
        Some(&self.case.native[*self.nodes.get(&(node.file.clone(), node.pin.clone()))?])
    }

    /// Supply exact relationship endpoints from previously verified declarations.
    fn projection(&self, captured_fact: &Fact) -> Option<(Pin, Option<&Fact>, Option<&Fact>)> {
        let mut pin = captured_fact
            .query
            .as_ref()
            .unwrap_or(&captured_fact.pin)
            .clone();
        let mut expected_owner = None;
        let mut expected_child = None;
        if captured_fact.owner.is_some()
            && self
                .case
                .captured
                .iter()
                .any(|symbol| symbol.pin.key[0] == "S")
        {
            if captured_fact
                .owner
                .as_ref()
                .is_some_and(|captured_owner| captured_owner[0] != "file")
            {
                let owner = self.endpoint(captured_fact, false)?;
                if pin.key[0] == "C" {
                    pin.key[1..3].clone_from_slice(&owner.pin.key[1..3]);
                } else {
                    pin.key[2..5].clone_from_slice(&owner.pin.key[1..4]);
                }
                expected_owner = Some(owner);
            }
            if pin.key[0] == "C" {
                let child = self.endpoint(captured_fact, true)?;
                pin.key[3..5].clone_from_slice(&child.pin.key[1..3]);
                expected_child = Some(child);
            }
        }
        Some((pin, expected_owner, expected_child))
    }

    /// Require a unique observation with exact typed owner and child identities.
    fn identity(&self, captured_fact: &Fact) -> Vec<usize> {
        let query = captured_fact.query.as_ref().unwrap_or(&captured_fact.pin);
        if captured_fact.pin.key[0] == "R"
            && self.case.captured.iter().any(|other_capture| {
                other_capture.file == captured_fact.file
                    && other_capture.query.as_ref().unwrap_or(&other_capture.pin) == query
                    && other_capture.pin != captured_fact.pin
            })
        {
            return Vec::new();
        }
        let Some((pin, expected_owner, expected_child)) = self.projection(captured_fact) else {
            return Vec::new();
        };
        let matches = if captured_fact.pin.end.is_some() {
            self.matches(&captured_fact.file, &pin)
        } else {
            self.base_identity(&captured_fact.file, &pin)
        };
        if matches.len() == 1 {
            let native = &self.case.native[matches[0]];
            if captured_fact
                .owner
                .as_ref()
                .is_some_and(|owner| owner[0] == "file")
                && native.owner.is_some()
                || expected_owner
                    .is_some_and(|owner| native.owner.as_ref() != Some(&concrete(&owner.pin)))
                || expected_child
                    .is_some_and(|child| native.target.as_ref() != Some(&concrete(&child.pin)))
            {
                return Vec::new();
            }
        }
        matches
    }

    /// Require every conjunctive pin and reject overlapping selected observations.
    fn aligned(&self, captured_fact: &Fact, pins: &[Pin]) -> Decision {
        let mut selected = BTreeSet::new();
        let mut problems = Vec::new();
        let id = fact_id(&self.case.language, captured_fact);
        for pin in pins {
            let found = self.matches(&captured_fact.file, pin);
            if let [observation_index] = found.as_slice() {
                if !selected.insert(*observation_index) {
                    problems.push(format!(
                        "overlapping pins for {id:?}: {pin:?} select native observation {observation_index}"
                    ));
                }
            } else {
                let near = suggestions(captured_fact, &self.case.native);
                problems.push(format!(
                    "missing/ambiguous pin for {id:?}: {pin:?}; native observations {found:?}{near}"
                ));
            }
        }
        // Partial groups cannot supply declaration correspondences to later facts.
        if !problems.is_empty() {
            selected.clear();
        }
        ("aligned".into(), selected.into_iter().collect(), problems)
    }

    /// Prefer unique identity, rejecting stale rows before considering an explicit policy.
    fn disposition(&self, captured_fact: &Fact, disposition: Option<&Disposition>) -> Decision {
        let matches = self.identity(captured_fact);
        let id = fact_id(&self.case.language, captured_fact);
        if matches.len() == 1 {
            let problems = disposition
                .map(|_| format!("stale/redundant {id:?}"))
                .into_iter()
                .collect();
            return ("identity".into(), matches, problems);
        }
        match disposition {
            Some(Aligned { v2, .. }) => self.aligned(captured_fact, v2),
            Some(Pending { wave, .. }) => (format!("pending-wave-{wave}"), Vec::new(), Vec::new()),
            Some(Intentional { id }) => (format!("intentional:{id}"), Vec::new(), Vec::new()),
            None => {
                let near = suggestions(captured_fact, &self.case.native);
                let error =
                    format!("unmatched/ambiguous {id:?}; native observations {matches:?}{near}");
                ("unmatched".into(), Vec::new(), vec![error])
            }
        }
    }

    /// Record a declaration correspondence only when exactly one symbol was selected.
    fn record_node(&mut self, captured_fact: &Fact, pins: &[usize]) {
        let mut symbols = pins
            .iter()
            .copied()
            .filter(|observation_index| self.case.native[*observation_index].pin.key[0] == "S");
        if let ("S", Some(symbol), None) = (
            captured_fact.pin.key[0].as_str(),
            symbols.next(),
            symbols.next(),
        ) {
            self.nodes.insert(
                (captured_fact.file.clone(), captured_fact.pin.clone()),
                symbol,
            );
        }
    }
}

/// Run the shared gate, verifying declarations before relationship endpoints.
fn check(cases: &[Case], policy: &Policy) -> Outcome {
    let mut outcome = Outcome::default();
    let rows = policy_rows(cases, policy, &mut outcome);
    for case in cases {
        let mut gate = Gate {
            case,
            index: index(&case.native),
            nodes: BTreeMap::new(),
        };
        let mut counted = BTreeSet::new();
        // Declarations are verified first; only verified correspondences can supply edge endpoints.
        let mut facts = case.captured.iter().collect::<Vec<_>>();
        facts.sort_by_key(|captured_fact| captured_fact.owner.is_some());
        for captured_fact in facts {
            let id = fact_id(&case.language, captured_fact);
            let disposition = rows.get(&id);
            let (status, pins, problems) = gate.disposition(captured_fact, disposition.copied());
            outcome.problems.extend(problems);
            gate.record_node(captured_fact, &pins);
            if counted.insert(id) {
                *outcome.counts.entry(status).or_default() += 1;
            }
            if !pins.is_empty() {
                outcome.carried.insert(
                    (
                        case.language.clone(),
                        captured_fact.file.clone(),
                        captured_fact.pin.clone(),
                    ),
                    (pins, matches!(disposition, Some(Aligned { .. }))),
                );
            }
        }
    }
    outcome
}

/// Require a complete clean gate result for a baseline extraction.
fn verified(cases: &[Case], policy: &Policy) -> Outcome {
    let outcome = check(cases, policy);
    assert!(
        outcome.problems.is_empty(),
        "{}",
        outcome.problems.join("\n")
    );
    outcome
}

/// Gate all frozen facts through identity, alignment or an explicit divergence.
#[test]
fn exact_parity_carries_every_captured_fact_or_an_explicit_divergence() {
    let (cases, policy) = corpus();
    let outcome = verified(cases, policy);
    assert_eq!(
        outcome.counts["intentional:v1-export-module-misattribution"],
        2
    );
    println!(
        "{FACT_COUNT} facts; 0 unmatched / 0 stale / 0 problems; {:?}",
        outcome.counts
    );
}

/// Require deterministic ordering and reject duplicate table records.
fn sorted<Record: DeserializeOwned + Ord>(file: &str) {
    assert!(
        jsonl::<Record>(file)
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "{file} order"
    );
}

/// Keep committed policy tables reviewable and deterministic.
#[test]
fn compact_tables_are_sorted() {
    sorted::<Alignment>("alignments.jsonl");
    sorted::<Row>("divergences.jsonl");
    sorted::<Definition>("reasons.jsonl");
}

/// Replay small cases and adversarial mutations through the production gate.
#[test]
fn small_inputs_and_review_counterexamples_exercise_the_real_gate() {
    for table in ["gate_cases.jsonl", "counterexamples.jsonl"] {
        for mut example in jsonl::<Example>(table) {
            if let Some(input) = example.input {
                assert_eq!(example.cases.len(), 1);
                let source = input.source.unwrap_or_else(|| {
                    read(
                        &Path::new(ROOT)
                            .join(&example.cases[0].language)
                            .join(&input.path),
                    )
                });
                let file = must(extract(&input.path, source.as_bytes()));
                example.cases[0].native = native(&file);
                verified(&example.cases, &example.policy);
                example.cases[0].native = native(&mutate(&file, input.mutations));
            }
            let problems = check(&example.cases, &example.policy).problems;
            assert!(
                example
                    .problems
                    .iter()
                    .all(|expected| problems.iter().any(|actual| actual.contains(expected)))
                    && (!example.problems.is_empty() || problems.is_empty()),
                "{}: {problems:?}",
                example.name
            );
            println!("caught {}", example.name);
        }
    }
}

/// Apply counterexample edits to real extraction output and remove dangling edges.
fn mutate(file: &ExtractedFile, mutations: Vec<Mutation>) -> ExtractedFile {
    let mut json = must(serde_json::to_value(file));
    // Edits operate on the production output shape, before readable projection.
    for mut mutation in mutations {
        if let Some(key) = mutation.symbol {
            let found = file
                .symbols
                .iter()
                .filter(|symbol| symbol_key(symbol) == key)
                .collect::<Vec<_>>();
            assert_eq!(found.len(), 1, "mutation symbol {key:?}");
            mutation.value = must(serde_json::to_value(&found[0].id));
        }
        let collection = json[&mutation.collection]
            .as_array_mut()
            .unwrap_or_else(|| panic!("mutation collection {}", mutation.collection));
        if let Some(field) = mutation.field {
            if field == "line" {
                let span = &mut collection[mutation.index]["span"];
                span["start"]["line"] = mutation.value.clone();
                span["end"]["line"] = mutation.value;
            } else {
                collection[mutation.index][field] = mutation.value;
            }
        } else {
            collection.remove(mutation.index);
        }
    }
    let mut file: ExtractedFile = must(serde_json::from_value(json));
    let present = |id: &SymbolId| file.symbols.iter().any(|symbol| &symbol.id == id);
    file.containments
        .retain(|containment| present(&containment.parent) && present(&containment.child));
    file.references
        .retain(|reference| reference.owner.as_ref().is_none_or(present));
    file
}
