use super::*;
use std::fmt::Write as _;

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let root = write_fixture_sources(fixtures);
    let forward = source_generation(fixtures, root.path(), false);
    let reverse = source_generation(fixtures, root.path(), true);
    assert_eq!(forward.digest(), reverse.digest());
    assert_eq!(forward.references(), reverse.references());
    assert_eq!(forward.edges(), reverse.edges());
    forward
}

fn write_fixture_sources(fixtures: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("source root: {error}"));
    for (path, source) in fixtures {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap_or(root.path()))
            .unwrap_or_else(|error| panic!("fixture directory: {error}"));
        std::fs::write(&path, source).unwrap_or_else(|error| panic!("fixture write: {error}"));
    }
    root
}

fn source_generation(
    fixtures: &[(&str, &str)],
    root: &std::path::Path,
    reverse: bool,
) -> CanonicalGenerationFacts {
    let accumulator = fixture_accumulator(fixtures, reverse);
    let source_root = SourceRoot::open(root).unwrap_or_else(|error| panic!("root: {error}"));
    let (facts, _) = resolve_generation(
        ResolveGenerationRequest {
            extracted: accumulator,
            maximum_bytes: TEST_GENERATION_BYTES,
            source_root,
            evidence_policy: FULL_TEST_EVIDENCE,
            clone_policy: NativeClonePolicy {
                wider_partial_band: false,
            },
        },
        || false,
    )
    .unwrap_or_else(|_| panic!("fixture resolution"));
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|_| panic!("validation limits"));
    validate_generation_facts(facts, limits, || false)
        .unwrap_or_else(|error| panic!("canonicalization: {error}"))
        .0
}

fn fixture_accumulator(fixtures: &[(&str, &str)], reverse: bool) -> NativeFactAccumulator {
    let source_limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limits: {error}"));
    let mut files = fixtures
        .iter()
        .map(|(path, source)| {
            let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
                path,
                source.as_bytes(),
                source_limits,
            )
            .unwrap_or_else(|error| panic!("fixture snapshot: {error}"));
            NativeExtractor::new_for_capability_validation(snapshot.language())
                .and_then(|mut extractor| extractor.extract(&snapshot))
                .unwrap_or_else(|error| panic!("fixture extraction: {error}"))
        })
        .collect::<Vec<_>>();
    if reverse {
        files.reverse();
    }
    let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
    for file in files {
        accumulator
            .push(file)
            .unwrap_or_else(|_| panic!("fixture accumulation"));
    }
    accumulator
}

fn fixture_resolution_index(
    fixtures: &[(&str, &str)],
) -> (tempfile::TempDir, NativeFactAccumulator, ResolutionIndex) {
    let root = write_fixture_sources(fixtures);
    let extracted = fixture_accumulator(fixtures, false);
    let source_root = SourceRoot::open(root.path()).unwrap_or_else(|error| panic!("root: {error}"));
    let maximum =
        resolve_reservation(MEGA_TEST_GENERATION_BYTES).unwrap_or_else(|_| panic!("reservation"));
    let mut budget = ResolveBudget::new(0, maximum).unwrap_or_else(|_| panic!("budget"));
    let index = build_resolution_index(
        &extracted,
        ResolutionIndexContext {
            source_root: &source_root,
            budget: &mut budget,
            cancelled: &mut || false,
        },
    )
    .unwrap_or_else(|_| {
        panic!(
            "resolution index: {} / {} budget bytes",
            budget.charged_bytes, maximum
        )
    });
    (root, extracted, index)
}

fn reference_request<'a>(
    file: &'a NativeFileFacts,
    reference: &'a ExtractedReference,
) -> ResolutionRequest<'a> {
    ResolutionRequest {
        file_id: &file.file.file_id,
        file_path: &file.file.normalized_path,
        language: &file.file.language,
        owner: reference.owner.as_ref(),
        name: &reference.name,
        kind: reference.kind,
        span: reference.span,
        dispatch: ReferenceDispatch::Static,
        import_bindings: ImportBindingSelection::empty(),
    }
}

fn assert_base_resolution(fixtures: &[(&str, &str)], selector: (&str, &str, ReferenceKind)) {
    let (_root, extracted, mut index) = fixture_resolution_index(fixtures);
    let (path, name, kind) = selector;
    let file = extracted
        .files
        .iter()
        .find(|file| file.file.normalized_path == path)
        .unwrap_or_else(|| panic!("file {path}"));
    let reference = file
        .references
        .iter()
        .find(|reference| reference.name == name && reference.kind == kind)
        .unwrap_or_else(|| panic!("reference {name}"));
    let positions = (0..file.import_bindings.len()).collect::<Vec<_>>();
    let mut request = reference_request(file, reference);
    request.import_bindings = ImportBindingSelection {
        bindings: &file.import_bindings,
        positions: &positions,
        fallback_blocked: false,
    };
    let added = resolve_reference(&index, &request, &mut || false)
        .unwrap_or_else(|_| panic!("added resolution"));
    index.qualtype = qualtype_resolution::TypeIndex::default();
    let base = resolve_reference(&index, &request, &mut || false)
        .unwrap_or_else(|_| panic!("base resolution"));
    assert_eq!(
        added.target.as_ref().map(|target| (
            &target.symbol_id,
            target.kind,
            target.confidence,
            target.provenance
        )),
        base.target.as_ref().map(|target| (
            &target.symbol_id,
            target.kind,
            target.confidence,
            target.provenance
        )),
    );
    assert_eq!(added.unresolved_provenance, base.unresolved_provenance);
}

fn assert_target(
    facts: &CanonicalGenerationFacts,
    reference: &ReferenceInput,
    target: (&str, &str, &str),
) {
    let (path, name, provenance) = target;
    let symbol = capability_symbol(facts, path, name);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&symbol.symbol_id),
        "{}",
        reference.reference_name
    );
    assert_eq!(reference.resolution_provenance, provenance);
}

#[test]
fn qualified_csharp_types_follow_using_and_same_namespace_without_global_fallback() {
    let facts = generation(&[
        (
            "Models/Item.cs",
            "namespace Shop.Models { public class Item {} public struct Price {} }",
        ),
        ("Other/Item.cs", "namespace Other { public class Item {} }"),
        (
            "Services/Local.cs",
            "namespace Services; public class Local {}",
        ),
        (
            "Services/Use.cs",
            "using System; using Shop.Models; namespace Services; public class Use { public Item Find(Price price) { return new Item(); } public Local FindLocal() { return new Local(); } public Missing Absent() { return new Missing(); } }",
        ),
    ]);
    let find = capability_symbol(&facts, "Services/Use.cs", "Services::Use::Find");
    for kind in [ReferenceKind::Returns, ReferenceKind::Instantiates] {
        let reference = CapabilityReferenceQuery::new(&facts, find).named("Item", kind);
        assert_target(
            &facts,
            reference,
            (
                "Models/Item.cs",
                "Shop.Models::Item",
                namespace_types::PROVENANCE,
            ),
        );
        assert_eq!(reference.confidence, 1.0);
    }
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, find).named("Price", ReferenceKind::TypeOf),
        (
            "Models/Item.cs",
            "Shop.Models::Price",
            namespace_types::PROVENANCE,
        ),
    );
    let local = capability_symbol(&facts, "Services/Use.cs", "Services::Use::FindLocal");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, local).named("Local", ReferenceKind::Returns),
        (
            "Services/Local.cs",
            "Services::Local",
            namespace_types::PROVENANCE,
        ),
    );
    let absent = capability_symbol(&facts, "Services/Use.cs", "Services::Use::Absent");
    assert!(
        CapabilityReferenceQuery::new(&facts, absent)
            .named("Missing", ReferenceKind::Returns)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn qualified_csharp_types_abstain_on_conflicting_usings_missing_usings_and_private_types() {
    let facts = generation(&[
        (
            "A.cs",
            "namespace A { public class Item {} private class Hidden {} }",
        ),
        ("B.cs", "namespace B { public class Item {} }"),
        (
            "Use.cs",
            "using A; using B; namespace Consumer; public class Use { public Item Conflict() { return null; } public Hidden Secret() { return null; } }",
        ),
        (
            "Absent.cs",
            "namespace Consumer; public class Absent { public Item Missing() { return null; } }",
        ),
    ]);
    for (path, owner, name) in [
        ("Use.cs", "Consumer::Use::Conflict", "Item"),
        ("Use.cs", "Consumer::Use::Secret", "Hidden"),
        ("Absent.cs", "Consumer::Absent::Missing", "Item"),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named(name, ReferenceKind::Returns)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn qualified_enum_members_bind_the_enum_parent_and_reject_unknown_receivers() {
    let source = "enum Step { One, Two } enum Other { Two } function read() { return Step.Two; } function unknown() { return Missing.Two; } const conditional = true ? Step.One : Step.Two; class Counter { value = 0; read() { return this.value; } }";
    for (path, wrapped) in [
        ("main.ts", source.to_owned()),
        (
            "Counter.svelte",
            format!("<script lang=\"ts\">{source}</script>"),
        ),
        (
            "Card.vue",
            format!("<script setup lang=\"ts\">{source}</script>"),
        ),
    ] {
        let facts = generation(&[(path, &wrapped)]);
        let read = capability_symbol(&facts, path, "read");
        let reference =
            CapabilityReferenceQuery::new(&facts, read).named("Two", ReferenceKind::FieldAccess);
        assert_target(
            &facts,
            reference,
            (path, "Step::Two", enum_resolution::PROVENANCE),
        );
        assert_eq!(reference.confidence, 1.0);
        let conditional = capability_symbol(&facts, path, "conditional");
        for (member, target) in [("One", "Step::One"), ("Two", "Step::Two")] {
            assert_target(
                &facts,
                CapabilityReferenceQuery::new(&facts, conditional)
                    .named(member, ReferenceKind::FieldAccess),
                (path, target, enum_resolution::PROVENANCE),
            );
        }
        let method = capability_symbol(&facts, path, "Counter::read");
        assert_target(
            &facts,
            CapabilityReferenceQuery::new(&facts, method)
                .named("value", ReferenceKind::FieldAccess),
            (path, "Counter::value", EXACT_LEXICAL_PROVENANCE),
        );
        let owner = capability_symbol(&facts, path, "unknown");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("Two", ReferenceKind::FieldAccess)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn enum_receiver_other_occurrences_preserve_base_resolution() {
    for source in [
        "enum Step { Two } const result = ((Step: { Two: number }) => Step.Two)({ Two: 99 });",
        "enum Step { Two } function read(Step: object) { return Step.Two; }",
        "enum Step { Two } const result = (() => { const Step = { Two: 99 }; return Step.Two; })();",
        "enum Step { Two } const obj = { Step: 99 }; const result = Step.Two;",
        "enum Step { Two } const copy = Step; const result = Step.Two;",
    ] {
        let fixtures = [("main.ts", source)];
        assert_base_resolution(&fixtures, ("main.ts", "Two", ReferenceKind::FieldAccess));
        let facts = generation(&fixtures);
        for reference in facts
            .references()
            .iter()
            .filter(|reference| reference.reference_name == "Two")
        {
            assert_ne!(reference.resolution_provenance, enum_resolution::PROVENANCE);
        }
    }
}

#[test]
fn java_enum_lookup_abstains_when_value_receiver_binding_is_unproven() {
    for declaration in [
        "Other read(Other Status) { return Status.ACTIVE; }",
        "Other Status; Other read() { return Status.ACTIVE; }",
    ] {
        let source = format!(
            "package demo; enum Status {{ ACTIVE }} enum Other {{ ACTIVE }} class Use {{ {declaration} }}"
        );
        let facts = generation(&[("Use.java", &source)]);
        let owner = capability_symbol(&facts, "Use.java", "demo::Use::read");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("ACTIVE", ReferenceKind::FieldAccess)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn rescript_qualified_types_identify_the_implementation_and_reject_duplicate_modules() {
    let fixtures = [
        ("src/Shapes.res", "type box<'a> = {value: 'a}\n"),
        ("src/Shapes.resi", "type box<'a> = {value: 'a}\n"),
        (
            "src/Utils.res",
            "let process = (input: Shapes.box<string>): Shapes.box<string> => input\nlet absent = (input: Missing.box<string>) => input\n",
        ),
    ];
    let facts = generation(&fixtures);
    let owner = capability_symbol(&facts, "src/Utils.res", "process");
    for reference in facts.references().iter().filter(|reference| {
        reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
            && reference.reference_name == "Shapes.box"
    }) {
        assert_target(
            &facts,
            reference,
            ("src/Shapes.res", "box", rescript_resolution::PROVENANCE),
        );
        assert_eq!(reference.confidence, 1.0);
    }
    assert_eq!(
        facts
            .references()
            .iter()
            .filter(
                |reference| reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && reference.reference_name == "Shapes.box"
            )
            .count(),
        3
    );
    let absent = capability_symbol(&facts, "src/Utils.res", "absent");
    assert!(
        CapabilityReferenceQuery::new(&facts, absent)
            .named("Missing.box", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
    let collision = [
        fixtures[0],
        fixtures[1],
        fixtures[2],
        ("other/Shapes.res", "type different = string\n"),
    ];
    let facts = generation(&collision);
    let owner = capability_symbol(&facts, "src/Utils.res", "process");
    assert!(
        facts
            .references()
            .iter()
            .filter(
                |reference| reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && reference.reference_name == "Shapes.box"
            )
            .all(|reference| reference.target_symbol_id.is_none())
    );
}

#[test]
fn rescript_interface_hides_implementation_only_types() {
    let facts = generation(&[
        ("Shapes.res", "type box<'a> = {value: 'a}\n"),
        ("Shapes.resi", "type visible = string\n"),
        (
            "Use.res",
            "let run = (value: Shapes.box<string>) => value\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.res", "run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Shapes.box", ReferenceKind::TypeOf);
    assert!(reference.target_symbol_id.is_none());
    assert_eq!(reference.resolution_provenance, UNRESOLVED_PROVENANCE);
    let facts = generation(&[
        ("Shapes.res", "type box = {value: string}\n"),
        (
            "Use.res",
            "module Shapes = { type box = {other: int} }\nlet run = (value: Shapes.box) => value\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.res", "run");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Shapes.box", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn codeigniter_controller_routes_bind_the_public_method_in_the_named_file() {
    let fixtures = [
        (
            "application/controllers/Welcome.php",
            "<?php function index() {} class Welcome extends CI_Controller { public function index() {} public function login() {} private function helper() {} }",
        ),
        (
            "application/controllers/Other.php",
            "<?php class Other extends CI_Controller { public function index() {} }",
        ),
        (
            "application/controllers/Unknown.php",
            "<?php class Different extends CI_Controller { public function index() {} }",
        ),
    ];
    let facts = generation(&fixtures);
    for (route, method) in [("/welcome", "index"), ("/welcome/login", "login")] {
        let owner = capability_symbol(
            &facts,
            "application/controllers/Welcome.php",
            &format!("application/controllers/Welcome.php::any::{route}"),
        );
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(method, ReferenceKind::Calls);
        assert_target(
            &facts,
            reference,
            (
                "application/controllers/Welcome.php",
                &format!("Welcome::{method}"),
                codeigniter_resolution::PROVENANCE,
            ),
        );
        assert_eq!(reference.confidence, 0.9);
    }
    assert_base_resolution(
        &fixtures,
        (
            "application/controllers/Unknown.php",
            "index",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn graphql_extensions_target_the_base_and_abstain_when_no_unique_base_exists() {
    let fixtures = [
        ("schema/base.graphql", "type User { id: ID! }"),
        ("schema/extensions.gql", "extend type User { owner: User }"),
        ("schema/operations.graphql", "type Query { me: User }"),
    ];
    let facts = generation(&fixtures);
    let extension = capability_symbol(&facts, "schema/extensions.gql", "User");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, extension).named("User", ReferenceKind::Extends),
        ("schema/base.graphql", "User", EXACT_PROJECT_PROVENANCE),
    );
    let owner = capability_symbol(&facts, "schema/extensions.gql", "User::owner");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("User", ReferenceKind::TypeOf),
        ("schema/base.graphql", "User", EXACT_PROJECT_PROVENANCE),
    );
    for fixtures in [
        vec![fixtures[1], fixtures[2]],
        vec![
            fixtures[0],
            fixtures[1],
            fixtures[2],
            ("duplicate.graphql", "type User { other: ID }"),
        ],
    ] {
        let facts = generation(&fixtures);
        let owner = capability_symbol(&facts, "schema/extensions.gql", "User");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("User", ReferenceKind::Extends)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn rust_inline_nominal_aliases_keep_the_full_module_identity() {
    let facts = generation(&[(
        "src/lib.rs",
        "mod inner { pub struct Slot { pub n: u32 } } mod other { pub struct Slot { pub n: u32 } } use self::inner::Slot; pub struct Store { slot: Slot } pub fn create() -> Slot { Slot { n: 1 } } pub fn absent() -> Missing { Missing {} }",
    )]);
    let owner = capability_symbol(&facts, "src/lib.rs", "create");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::Instantiates),
        ("src/lib.rs", "inner::Slot", rust_local_types::PROVENANCE),
    );
    let owner = capability_symbol(&facts, "src/lib.rs", "Store");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::TypeOf),
        ("src/lib.rs", "inner::Slot", rust_local_types::PROVENANCE),
    );
    let owner = capability_symbol(&facts, "src/lib.rs", "absent");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Missing", ReferenceKind::Instantiates)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn csharp_root_type_aliases_and_global_names_bind_the_exact_namespace() {
    let facts = generation(&[
        ("A.cs", "namespace A { public class Item {} }"),
        ("B.cs", "namespace B { public class Item {} }"),
        (
            "Use.cs",
            "using I = A.Item; namespace Consumer; public class Use { public global::A.Item Namespaced() { return null; } public I Aliased() { return null; } public global::B.Item Explicit() { return null; } }",
        ),
    ]);
    for (owner, name, target) in [
        ("Namespaced", "A.Item", "A"),
        ("Aliased", "I", "A"),
        ("Explicit", "B.Item", "B"),
    ] {
        let owner = capability_symbol(&facts, "Use.cs", &format!("Consumer::Use::{owner}"));
        assert_target(
            &facts,
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Returns),
            (
                &format!("{target}.cs"),
                &format!("{target}::Item"),
                namespace_types::PROVENANCE,
            ),
        );
    }
}

#[test]
fn python_typevar_types_require_the_explicit_typing_factory() {
    let fixtures = [
        (
            "models.py",
            "from typing import TypeVar\nT = TypeVar('T')\n",
        ),
        (
            "use.py",
            "from typing import Generic\nfrom models import T\nclass Box(Generic[T]):\n    item: T\n",
        ),
    ];
    let facts = generation(&fixtures);
    let owner = capability_symbol(&facts, "use.py", "Box");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("T", ReferenceKind::TypeOf),
        ("models.py", "T", python_type_variables::PROVENANCE),
    );
    for declaration in [
        "T = object()",
        "from typing import TypeVar\ndef TypeVar(name):\n    pass\nT = TypeVar('T')",
        "from typing import TypeVar\nT = [TypeVar('T')]",
        "from typing import TypeVar\nT = 0 + TypeVar('T')",
    ] {
        let facts = generation(&[("models.py", declaration), fixtures[1]]);
        let owner = capability_symbol(&facts, "use.py", "Box");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("T", ReferenceKind::TypeOf)
                .target_symbol_id
                .is_none()
        );
    }
    let facts = generation(&[
        fixtures[0],
        fixtures[1],
        ("typing.py", "def TypeVar(name):\n    return object()\n"),
    ]);
    let owner = capability_symbol(&facts, "use.py", "Box");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("T", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn csharp_same_class_construction_selects_a_unique_constructor_and_abstains_on_overloads() {
    let source = "namespace Shop; public struct Money { public Money(decimal value) {} public static Money Zero() => new Money(0); }";
    let facts = generation(&[("Money.cs", source)]);
    let owner = capability_symbol(&facts, "Money.cs", "Shop::Money::Zero");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("Money", ReferenceKind::Instantiates),
        (
            "Money.cs",
            "Shop::Money::Money",
            csharp_constructors::PROVENANCE,
        ),
    );
    for source in [
        "public struct Money { public Money(decimal value) {} public static Money Zero() => new Money(); }",
        "public struct Money { public Money(decimal value) {} public static Money Zero() => new Money(1, 2); }",
        "public struct Money { public Money(decimal value) {} public Money(int value) {} public static Money Zero() => new Money(0); }",
    ] {
        let fixtures = [("Money.cs", source)];
        assert_base_resolution(
            &fixtures,
            ("Money.cs", "Money", ReferenceKind::Instantiates),
        );
        let facts = generation(&fixtures);
        let owner = capability_symbol(&facts, "Money.cs", "Money::Zero");
        assert_target(
            &facts,
            CapabilityReferenceQuery::new(&facts, owner)
                .named("Money", ReferenceKind::Instantiates),
            ("Money.cs", "Money", EXACT_SAME_FILE_PROVENANCE),
        );
    }
}

#[test]
fn csharp_unrelated_generic_owner_preserves_the_base_type_target() {
    let fixtures = [(
        "Use.cs",
        "public class Item {} public class Use<T> { public Item Make() => new Item(); }",
    )];
    assert_base_resolution(&fixtures, ("Use.cs", "Item", ReferenceKind::Instantiates));
    let facts = generation(&fixtures);
    let owner = capability_symbol(&facts, "Use.cs", "Use::Make");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("Item", ReferenceKind::Instantiates),
        ("Use.cs", "Item", EXACT_SAME_FILE_PROVENANCE),
    );
}

#[test]
fn csharp_construction_does_not_select_a_static_initializer() {
    let facts = generation(&[(
        "Money.cs",
        "namespace Shop; public class Money { static Money() {} public static Money Zero() => new Money(); }",
    )]);
    let owner = capability_symbol(&facts, "Money.cs", "Shop::Money::Zero");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Money", ReferenceKind::Instantiates);
    let initializer = capability_symbol(&facts, "Money.cs", "Shop::Money::Money");
    assert_ne!(
        reference.target_symbol_id.as_ref(),
        Some(&initializer.symbol_id)
    );
    assert_target(
        &facts,
        reference,
        ("Money.cs", "Shop::Money", EXACT_LEXICAL_PROVENANCE),
    );
}

#[test]
fn csharp_qualified_types_preserve_base_resolution_on_type_and_generic_shadowing() {
    for (declaration, name, kind) in [
        (
            "public class Model { public class Item {} } public class Use { public Model.Item Read() => null; }",
            "Model.Item",
            ReferenceKind::Returns,
        ),
        (
            "public class Use<Item> { public Item Read() => default; }",
            "Item",
            ReferenceKind::Returns,
        ),
        (
            "[Dummy(\"]Use()\")] public class Use<Item> { public Item Read() => default; }",
            "Item",
            ReferenceKind::Returns,
        ),
        (
            "public class Money { public Money() {} public Money Create<Money>() where Money : new() => new Money(); }",
            "Money",
            ReferenceKind::Instantiates,
        ),
    ] {
        let source = format!("using Model; namespace Client {{ {declaration} }}");
        assert_base_resolution(
            &[
                ("Model.cs", "namespace Model { public class Item {} }"),
                ("Use.cs", &source),
            ],
            ("Use.cs", name, kind),
        );
    }
}

#[test]
fn csharp_attributes_and_generic_return_types_preserve_nominal_lookup() {
    let facts = generation(&[
        ("Model.cs", "namespace Model { public class Item {} }"),
        (
            "Use.cs",
            "using Model; namespace Client { [Dummy(\"]\")] public class Use { [HttpGet(\"{id}\")] public Task<Item> Read() => null; } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.cs", "Client::Use::Read");
    assert_target(
        &facts,
        CapabilityReferenceQuery::new(&facts, owner).named("Item", ReferenceKind::Returns),
        ("Model.cs", "Model::Item", namespace_types::PROVENANCE),
    );
}

#[test]
fn unverified_source_does_not_add_inline_type_targets() {
    let source =
        "mod inner { pub struct Slot {} } use self::inner::Slot; pub struct Use { value: Slot }";
    for bytes in [source.replace("Slot", "Lost"), " ".repeat(4096)] {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("root: {error}"));
        std::fs::write(root.path().join("lib.rs"), bytes)
            .unwrap_or_else(|error| panic!("source: {error}"));
        let facts = source_generation(&[("lib.rs", source)], root.path(), false);
        let owner = capability_symbol(&facts, "lib.rs", "Use");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("Slot", ReferenceKind::TypeOf)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn python_typevar_factory_honors_import_and_parameter_fences() {
    for source in [
        "from typing import TypeVar\nfrom helpers import *\nT = TypeVar('T')\nclass Box:\n    def value(self, x: T) -> T: return x\n",
        "from typing import TypeVar\ndef factory(TypeVar):\n    T = TypeVar('T')\n    class Box:\n        def value(self, x: T) -> T: return x\n",
    ] {
        let facts = generation(&[
            ("models.py", source),
            ("helpers.py", "def TypeVar(name): return object()\n"),
        ]);
        let reference = facts
            .references()
            .iter()
            .find(|reference| {
                reference.reference_name == "T"
                    && reference.reference_kind == ReferenceKind::TypeOf.as_str()
            })
            .unwrap_or_else(|| panic!("type variable occurrence"));
        assert!(reference.target_symbol_id.is_none(), "{source}");
    }
}

#[test]
fn swift_objc_type_requires_explicit_bridge_evidence() {
    let facts = generation(&[
        (
            "Worker.m",
            "@interface Worker : NSObject\n@end\n@implementation Worker\n@end\n",
        ),
        (
            "Use.swift",
            "import Foundation\nfunc useObjc(worker: Worker) {}\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.swift", "useObjc");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Worker", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn csharp_namespace_scoped_usings_preserve_base_resolution() {
    for directive in ["using Model;", "using M = Model;"] {
        let name = if directive.contains('=') {
            "M.Item"
        } else {
            "Item"
        };
        let source = format!(
            "namespace A {{ {directive} public class Local {{}} }} namespace Consumer {{ public class Use {{ public {name} Missing() => null; }} }}"
        );
        assert_base_resolution(
            &[
                ("Model.cs", "namespace Model { public class Item {} }"),
                ("Use.cs", &source),
            ],
            ("Use.cs", name, ReferenceKind::Returns),
        );
    }
}

#[test]
fn csharp_nearer_using_scope_disables_the_new_namespace_lookup() {
    let fixtures = [
        ("A.cs", "namespace A { public class Item {} }"),
        ("B.cs", "namespace B { public class Item {} }"),
        (
            "Use.cs",
            "using A; namespace C { using B; public class Use { public Item Read() => null; } }",
        ),
    ];
    assert_base_resolution(&fixtures, ("Use.cs", "Item", ReferenceKind::Returns));
    let facts = generation(&fixtures);
    let owner = capability_symbol(&facts, "Use.cs", "C::Use::Read");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Item", ReferenceKind::Returns);
    assert!(reference.target_symbol_id.is_none());
    assert_eq!(reference.resolution_provenance, UNRESOLVED_PROVENANCE);
}

#[test]
fn csharp_relative_qualifier_namespaces_preserve_base_and_global_qualification_is_exact() {
    for (scope, nested) in [
        ("Client", "namespace Client.A { public class Item {} }"),
        ("X.Y", "namespace X.A { public class Item {} }"),
        ("X.Y", "namespace X.Y.A { public class Item {} }"),
        (
            "Client",
            "namespace Client.A.Nested { public class Other {} }",
        ),
        (
            "Client",
            "namespace Client { public class A { public class Item {} } }",
        ),
    ] {
        let source = format!(
            "namespace {scope} {{ public class Use {{ public A.Item Relative() => null; public global::A.Item Exact() => null; }} }}"
        );
        let fixtures = [
            ("A.cs", "namespace A { public class Item {} }"),
            ("Nested.cs", nested),
            ("Use.cs", source.as_str()),
        ];
        assert_base_resolution(&fixtures, ("Use.cs", "A.Item", ReferenceKind::Returns));
        let facts = generation(&fixtures);
        let owner = capability_symbol(&facts, "Use.cs", &format!("{scope}::Use::Exact"));
        assert_target(
            &facts,
            CapabilityReferenceQuery::new(&facts, owner).named("A.Item", ReferenceKind::Returns),
            ("A.cs", "A::Item", namespace_types::PROVENANCE),
        );
    }
}

#[test]
fn csharp_enclosing_and_root_types_preserve_base_before_imports() {
    for (scope, declaration) in [
        ("X.Y", "namespace X { public class Item {} }"),
        ("Client", "public class Item {}"),
    ] {
        let source = format!(
            "using A; namespace {scope} {{ public class Use {{ public Item Read() => null; public Local Direct() => null; }} }}"
        );
        let local = format!("namespace {scope} {{ public class Local {{}} }}");
        let fixtures = [
            ("A.cs", "namespace A { public class Item {} }"),
            ("Parent.cs", declaration),
            ("Local.cs", local.as_str()),
            ("Use.cs", source.as_str()),
        ];
        assert_base_resolution(&fixtures, ("Use.cs", "Item", ReferenceKind::Returns));
        let facts = generation(&fixtures);
        let owner = capability_symbol(&facts, "Use.cs", &format!("{scope}::Use::Direct"));
        assert_target(
            &facts,
            CapabilityReferenceQuery::new(&facts, owner).named("Local", ReferenceKind::Returns),
            (
                "Local.cs",
                &format!("{scope}::Local"),
                namespace_types::PROVENANCE,
            ),
        );
    }
}

#[test]
fn csharp_global_root_type_requests_preserve_base_resolution() {
    let fixtures = [
        ("Root.cs", "public class Item {}"),
        ("Local.cs", "namespace Client { public class Item {} }"),
        (
            "Use.cs",
            "namespace Client { public class Use { public global::Item Read() => null; } }",
        ),
    ];
    assert_base_resolution(&fixtures, ("Use.cs", "Item", ReferenceKind::Returns));
}

#[test]
fn csharp_namespace_import_lookup_polls_cancellation_on_missing_keys() {
    let mut source = String::new();
    let mut owned = Vec::new();
    for number in 0..100 {
        write!(source, "using N{number}; ")
            .unwrap_or_else(|error| panic!("using fixture: {error}"));
        owned.push((
            format!("N{number}.cs"),
            format!("namespace N{number} {{ public class Other {{}} }}"),
        ));
    }
    source.push_str("namespace Consumer { public class Use { public Item Read() => null; } }");
    owned.push(("Use.cs".to_owned(), source));
    owned.push((
        "Decoy.cs".to_owned(),
        "namespace Decoy { public class Item {} }".to_owned(),
    ));
    let fixtures = owned
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (_root, extracted, index) = fixture_resolution_index(&fixtures);
    let file = extracted
        .files
        .iter()
        .find(|file| file.file.normalized_path == "Use.cs")
        .unwrap_or_else(|| panic!("consumer"));
    let reference = file
        .references
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Returns)
        .unwrap_or_else(|| panic!("type reference"));
    let request = reference_request(file, reference);
    let mut polls = 0;
    assert!(
        resolve_reference(&index, &request, &mut || {
            polls += 1;
            polls > 32
        })
        .is_err()
    );
    assert_eq!(polls, 33);
}

#[test]
fn csharp_namespace_lookup_work_is_linear_in_the_reference_count() {
    const COUNT: usize = 1000;
    let mut owned = (0..COUNT)
        .map(|number| {
            (
                format!("N{number}.cs"),
                format!("namespace N{number} {{ public class Item {{}} }}"),
            )
        })
        .collect::<Vec<_>>();
    let mut source = "using N0; namespace Consumer { public class Use {".to_owned();
    for number in 0..COUNT {
        write!(source, " public Item Read{number}() => null;")
            .unwrap_or_else(|error| panic!("method fixture: {error}"));
    }
    source.push_str(" } }");
    owned.push(("Use.cs".to_owned(), source));
    let fixtures = owned
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (_root, extracted, index) = fixture_resolution_index(&fixtures);
    let target = extracted
        .files
        .iter()
        .flat_map(|file| &file.symbols)
        .find(|symbol| symbol.input.qualified_name == "N0::Item")
        .unwrap_or_else(|| panic!("target"));
    let file = extracted
        .files
        .iter()
        .find(|file| file.file.normalized_path == "Use.cs")
        .unwrap_or_else(|| panic!("consumer"));
    let mut visits = 0_usize;
    let mut resolved = 0;
    for reference in file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Returns)
    {
        let request = reference_request(file, reference);
        let resolution = resolve_reference(&index, &request, &mut || {
            visits += 1;
            false
        })
        .unwrap_or_else(|_| panic!("resolution"));
        let actual = resolution.target.unwrap_or_else(|| panic!("unresolved"));
        assert_eq!(actual.symbol_id, target.input.symbol_id);
        assert_eq!(actual.provenance, namespace_types::PROVENANCE);
        resolved += 1;
    }
    assert_eq!(resolved, COUNT);
    // Each cancellation poll marks an owner or candidate visit. A short-name
    // bucket scan per reference would exceed this fixed linear work budget.
    assert!(
        visits <= 16 * COUNT,
        "{visits} visits for {COUNT} references"
    );
}

#[test]
fn rust_inline_types_reject_private_targets_and_nested_import_scopes() {
    for source in [
        "mod inner { struct Slot {} } use self::inner::Slot; pub struct Use { value: Slot }",
        "mod inner { pub struct Slot {} } mod other { use self::inner::Slot; } pub struct Use { value: Slot }",
        "mod inner { pub struct Slot {} } use self::inner::Slot; pub struct Use<Slot> { value: Slot }",
        "mod inner { pub struct Slot {} } use self::inner::Slot; #[attr(r#\"\"] pub struct Use {\"#)] pub struct Use<Slot> { value: Slot }",
        "mod inner { pub struct Slot {} } use self::inner::Slot; #[attr(br#\"\"] pub struct Use {\"#)] pub struct Use<Slot> { value: Slot }",
    ] {
        let facts = generation(&[("lib.rs", source)]);
        let owner = capability_symbol(&facts, "lib.rs", "Use");
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("Slot", ReferenceKind::TypeOf)
                .target_symbol_id
                .is_none()
        );
    }
    let facts = generation(&[(
        "lib.rs",
        "mod inner { pub struct Slot {} } use self::inner::Slot; mod other { pub struct Use { value: Slot } }",
    )]);
    let owner = capability_symbol(&facts, "lib.rs", "other::Use");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Slot", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_attribute_lifetime_tokens_cannot_hide_a_generic_binding() {
    let facts = generation(&[(
        "lib.rs",
        "mod inner { pub struct Slot {} } use self::inner::Slot; type f = (); #[attr('a)] pub fn f<Slot>(v: [&'static Slot; 1]) -> f {}",
    )]);
    let owner = CapabilitySymbolQuery::new(&facts, "lib.rs").of_kind("f", SymbolKind::Function);
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Slot", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_impl_members_do_not_inherit_the_struct_declaration_scope() {
    let source = "mod inner { pub struct Slot {} } use self::inner::Slot; struct Store; trait Convert<T> { fn create(value: T); } impl<Slot> Convert<Slot> for Store { fn create(value: Slot) {} }";
    for declaration in ["struct Store;", "use external::Store;"] {
        let source = source.replace("struct Store;", declaration);
        let facts = generation(&[("lib.rs", &source)]);
        let owner = CapabilitySymbolQuery::new(&facts, "lib.rs")
            .of_kind("Store::create", SymbolKind::Method);
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("Slot", ReferenceKind::TypeOf)
                .target_symbol_id
                .is_none()
        );
    }
}
