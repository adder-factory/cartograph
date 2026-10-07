use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, FULL_TEST_EVIDENCE,
    NativeClonePolicy, NativeExtractor, NativeFactAccumulator, PipelineStage, ReferenceKind,
    ResolveGenerationRequest, SourceLimits, SourceRoot, TEST_GENERATION_BYTES, TEST_SOURCE_BYTES,
    assert_confidence, build_capability_generation, capability_symbol,
    generation_validation_limits, resolve_generation, validate_generation_facts,
};
use std::fs;
use tempfile::tempdir;

pub(super) fn base_generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    build_generation(
        CapabilityGenerationRequest {
            fixtures,
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: TEST_GENERATION_BYTES,
        },
        |file| file.call_scope_sites.clear(),
        || false,
    )
}

pub(super) fn assert_base_reference(
    base: &CanonicalGenerationFacts,
    reference: &super::ReferenceInput,
) {
    let original = base
        .references()
        .iter()
        .find(|original| {
            original.file_id == reference.file_id
                && original.owner_symbol_id == reference.owner_symbol_id
                && original.start_byte == reference.start_byte
                && original.end_byte == reference.end_byte
                && original.reference_name == reference.reference_name
                && original.reference_kind == reference.reference_kind
        })
        .unwrap_or_else(|| panic!("reference exists in the base generation"));
    assert_eq!(reference, original);
}

#[test]
fn explicit_typescript_this_parameter_preserves_base_resolution() {
    let fixtures = [(
        "src/worker.ts",
        "class Other { helper() {} } class LocalWorker { helper() {} run(this: Other) { this.helper(); } }",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/worker.ts", "LocalWorker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn csharp_pattern_binding_abstains_and_preserves_base_resolution() {
    let fixtures = [(
        "src/Worker.cs",
        "class Worker { void helper() {} void run(object value) { if (value is System.Action helper) helper(); } }",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/Worker.cs", "Worker::run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn vbnet_folded_loop_binding_abstains_and_preserves_base_resolution() {
    let fixtures = [(
        "src/worker.vb",
        "Public Class Worker\nPublic Sub Run(callbacks As System.Action())\nFor Each process In callbacks\nPROCESS()\nNext\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/worker.vb", "Worker::Run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn javascript_with_body_abstains_from_recursion_and_proximity() {
    let fixtures = [
        (
            "src/feature/use.js",
            "function retry(callbacks) { with (callbacks) { retry(); nearby(); } }",
        ),
        ("src/feature/api.js", "export function nearby() {}"),
        ("src/elsewhere/api.js", "export function nearby() {}"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/feature/use.js", "retry");
    for name in ["retry", "nearby"] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
    assert!(
        !facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
            && edge.source_symbol_id == owner.symbol_id
            && edge.target_symbol_id == owner.symbol_id)
    );
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn included_cpp_alias_blocks_a_project_qualified_suffix() {
    let fixtures = [
        (
            "src/alias.hpp",
            "namespace actual { class Target { public: static void run() {} }; } using Foo = actual::Target;",
        ),
        (
            "src/use.cpp",
            "#include \"alias.hpp\"\nvoid use() { Foo::run(); }",
        ),
        (
            "src/other.cpp",
            "namespace company { class Foo { public: static void run() {} }; }",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let alias = capability_symbol(&facts, "src/alias.hpp", "Foo");
    assert_eq!(alias.symbol_kind, "type_alias");
    let owner = capability_symbol(&facts, "src/use.cpp", "use");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("Foo::run", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn project_alias_declarations_block_suffixes_without_an_include() {
    for declaration in [
        "using Foo = actual::Target;",
        "typedef actual::Target Foo;",
        "namespace Foo = actual;",
    ] {
        let aliases = format!("namespace actual {{ class Target {{}} }} {declaration}");
        let fixtures = [
            ("src/alias.cpp", aliases.as_str()),
            ("src/use.cpp", "void use() { Foo::run(); }"),
            (
                "src/other.cpp",
                "namespace company { class Foo { public: static void run() {} }; }",
            ),
        ];
        let facts = build_capability_generation(&fixtures, false);
        let alias = capability_symbol(&facts, "src/alias.cpp", "Foo");
        assert_eq!(alias.symbol_kind, "type_alias", "{declaration}");
        let owner = capability_symbol(&facts, "src/use.cpp", "use");
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("Foo::run", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{declaration}: {call:?}");
    }
}

#[test]
fn unrelated_swift_local_preserves_exact_sibling_resolution() {
    let fixtures = [(
        "src/worker.swift",
        "class Worker { func helper() {} func run() { let count = 1; helper() } }",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/worker.swift", "Worker::run");
    let target = capability_symbol(&facts, "src/worker.swift", "Worker::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-exact-lexical");
    assert_confidence(call.confidence, 1.0);
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

#[test]
fn sql_trigger_preserves_exact_function_resolution() {
    let fixtures = [(
        "src/trigger.sql",
        "CREATE FUNCTION audit_fn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql; CREATE TABLE items(id integer); CREATE TRIGGER audit BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit_fn();",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/trigger.sql", "audit");
    let target = capability_symbol(&facts, "src/trigger.sql", "audit_fn");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("audit_fn", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-exact-same-file");
    assert_confidence(call.confidence, 1.0);
    assert_eq!(facts.digest(), base_generation(&fixtures).digest());
}

fn measured_generation(source: &str) -> (CanonicalGenerationFacts, usize) {
    let mut polls = 0_usize;
    let facts = build_generation(
        CapabilityGenerationRequest {
            fixtures: &[("src/Workers.java", source)],
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: TEST_GENERATION_BYTES,
        },
        |_| {},
        || {
            polls += 1;
            false
        },
    );
    (facts, polls)
}

#[test]
fn two_thousand_class_receiver_calls_have_linear_resolution_work() {
    const CLASSES: usize = 2_000;
    // Whitespace admits this dense generated input under the normal per-file
    // output budget; the classes and calls retain the verifier's shape.
    let padding = " ".repeat(128);
    let mut source = String::new();
    for ordinal in 0..CLASSES {
        super::append_fixture_text(
            &mut source,
            format_args!(
                "{padding}class C{ordinal} {{ void run() {{ this.helper(); }} void helper() {{}} }}\n"
            ),
        );
    }
    let (facts, polls) = measured_generation(&source);
    // Every candidate visit polls cancellation, including the former N*N scan.
    // The complete resolver includes clone/graph analysis and canonical facts;
    // it must stay within 1,024 polls per class and double approximately linearly.
    assert!(
        polls < CLASSES * 1_024,
        "{polls} cancellation polls for {CLASSES} classes"
    );
    let half_source = source
        .lines()
        .take(CLASSES / 2)
        .collect::<Vec<_>>()
        .join("\n");
    let (_, half_polls) = measured_generation(&half_source);
    assert!(
        polls <= half_polls * 21 / 10,
        "resolution work grew from {half_polls} to {polls}"
    );
    eprintln!("receiver workload: {half_polls} polls for 1,000 classes; {polls} for 2,000");
    let symbols = facts
        .symbols()
        .iter()
        .map(|symbol| (&symbol.symbol_id, symbol))
        .collect::<std::collections::HashMap<_, _>>();
    let mut calls = 0;
    for call in facts.references().iter().filter(|reference| {
        reference.reference_kind == ReferenceKind::Calls.as_str()
            && reference.reference_name == "this.helper"
    }) {
        let owner = symbols[&call
            .owner_symbol_id
            .clone()
            .unwrap_or_else(|| panic!("missing class method owner"))];
        let target = symbols[&call
            .target_symbol_id
            .clone()
            .unwrap_or_else(|| panic!("missing exact receiver target"))];
        let class = owner
            .qualified_name
            .strip_suffix("::run")
            .unwrap_or_else(|| panic!("missing run method"));
        assert_eq!(target.qualified_name, format!("{class}::helper"));
        assert_eq!(target.symbol_kind, "method");
        assert_eq!(call.resolution_provenance, "native-current-class-call");
        assert_confidence(call.confidence, 1.0);
        calls += 1;
    }
    assert_eq!(calls, CLASSES);
    assert_eq!(
        facts
            .edges()
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls
                && edge.provenance == "native-current-class-call")
            .count(),
        CLASSES
    );
}

#[derive(Clone, Copy)]
pub(super) struct CapabilityGenerationRequest<'a> {
    pub(super) fixtures: &'a [(&'a str, &'a str)],
    pub(super) reverse: bool,
    pub(super) wider_partial_band: bool,
    pub(super) maximum_bytes: u64,
}

pub(super) fn build_generation<Transform, Cancel>(
    request: CapabilityGenerationRequest<'_>,
    mut transform: Transform,
    cancelled: Cancel,
) -> CanonicalGenerationFacts
where
    Transform: FnMut(&mut cartograph_extract::ExtractedFile),
    Cancel: FnMut() -> bool,
{
    // Fixtures also exist on disk: configuration resolution verifies file bytes.
    let directory = tempdir().unwrap_or_else(|error| panic!("capability directory: {error}"));
    for (path, source) in request.fixtures {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap_or(directory.path()))
            .unwrap_or_else(|error| panic!("capability parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("capability source: {error}"));
    }
    let source_limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("capability source limits failed: {error}"));
    let mut extracted = request
        .fixtures
        .iter()
        .map(|(path, source)| {
            let snapshot =
                cartograph_extract::SourceSnapshot::from_bytes_for_capability_validation(
                    path,
                    source.as_bytes(),
                    source_limits,
                )
                .unwrap_or_else(|error| panic!("capability snapshot failed for {path}: {error}"));
            NativeExtractor::new_for_capability_validation(snapshot.language())
                .and_then(|mut extractor| extractor.extract(&snapshot))
                .unwrap_or_else(|error| panic!("capability extraction failed for {path}: {error}"))
        })
        .collect::<Vec<_>>();
    if request.reverse {
        extracted.reverse();
    }
    let mut accumulator = NativeFactAccumulator::new(request.maximum_bytes);
    for mut file in extracted {
        transform(&mut file);
        accumulator
            .push(file)
            .unwrap_or_else(|_| panic!("capability facts exceeded the modeled input limit"));
    }
    let (facts, _) = resolve_generation(
        ResolveGenerationRequest {
            extracted: accumulator,
            maximum_bytes: request.maximum_bytes,
            source_root: SourceRoot::open(directory.path())
                .unwrap_or_else(|error| panic!("capability root: {error}")),
            evidence_policy: FULL_TEST_EVIDENCE,
            clone_policy: NativeClonePolicy {
                wider_partial_band: request.wider_partial_band,
            },
        },
        cancelled,
    )
    .unwrap_or_else(|_| panic!("capability resolution exceeded its declared budget"));
    let validation_limits =
        generation_validation_limits(request.maximum_bytes, PipelineStage::Reduce)
            .unwrap_or_else(|error| panic!("capability validation limits failed: {error}"));
    validate_generation_facts(facts, validation_limits, || false).map_or_else(
        |error| panic!("capability canonicalization failed: {error}"),
        |(facts, _)| facts,
    )
}
