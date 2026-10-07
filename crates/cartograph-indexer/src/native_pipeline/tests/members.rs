use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceKind, build_capability_generation,
    capability_symbol,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let facts = build_capability_generation(fixtures, false);
    let reverse = build_capability_generation(fixtures, true);
    assert_eq!(facts.digest(), reverse.digest());
    assert_eq!(facts.references(), reverse.references());
    facts
}

fn extract_fixture(path: &str, source: &str) -> cartograph_extract::ExtractedFile {
    let snapshot = cartograph_extract::SourceSnapshot::from_bytes(
        path,
        source.as_bytes(),
        super::SourceLimits::new(super::TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("source limit: {error}")),
    )
    .unwrap_or_else(|error| panic!("snapshot: {error}"));
    super::NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("extractor: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extract: {error}"))
}

fn target(
    facts: &CanonicalGenerationFacts,
    call: (&str, &str, &str),
    expected: (&str, &str, &str),
) {
    let owner = capability_symbol(facts, call.0, call.1);
    let target = capability_symbol(facts, expected.0, expected.1);
    let reference = CapabilityReferenceQuery::new(facts, owner).named(call.2, ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{reference:?}"
    );
    assert_eq!(reference.resolution_provenance, expected.2);
}

#[test]
fn imported_static_members_preserve_exact_base_targets_and_private_abstentions() {
    let facts = generation(&[
        (
            "src/builder.ts",
            "export class Builder { static create() {} private static hidden() {} }",
        ),
        (
            "src/use.ts",
            "import { Builder as Factory } from './builder'; export function run() { Factory.create(); Factory.hidden(); Factory.missing(); }",
        ),
        (
            "src/decoy.ts",
            "export class Builder { static create() {} }",
        ),
    ]);
    target(
        &facts,
        ("src/use.ts", "run", "Factory.create"),
        ("src/builder.ts", "Builder::create", "native-import-binding"),
    );
    let owner = capability_symbol(&facts, "src/use.ts", "run");
    for name in ["Factory.hidden", "Factory.missing"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn apex_and_dart_current_class_calls_use_only_their_enclosing_class() {
    for (path, source, explicit) in [
        (
            "Worker.cls",
            "public class Worker { void run() { helper(); this.helper2(); } private void helper() {} private void helper2() {} } public class Other { void helper() {} }",
            "this.helper2",
        ),
        (
            "worker.dart",
            "class Worker { void run() { helper(); this.helper2(); } void helper() {} void helper2() {} } class Other { void helper() {} }",
            "helper2",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        for name in ["helper", explicit] {
            target(
                &facts,
                (path, "Worker::run", name),
                (
                    path,
                    if name == "helper" {
                        "Worker::helper"
                    } else {
                        "Worker::helper2"
                    },
                    "native-current-class-call",
                ),
            );
        }
    }
}

#[test]
fn new_current_class_rules_abstain_for_shadowed_and_missing_methods() {
    let facts = generation(&[
        (
            "Worker.cls",
            "public class Worker { void run() { missing(); } } public class Other { void missing() {} }",
        ),
        (
            "worker.dart",
            "class Worker { void run(void Function() helper) { helper(); missing(); } void helper() {} } class Other { void missing() {} }",
        ),
    ]);
    for (path, name) in [
        ("Worker.cls", "missing"),
        ("worker.dart", "helper"),
        ("worker.dart", "missing"),
    ] {
        let owner = capability_symbol(&facts, path, "Worker::run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn groovy_methods_keep_class_scope_and_delegate_closures_abstain() {
    let facts = generation(&[(
        "Worker.groovy",
        "package p\nclass Worker { void run() { helper(); this.audit(); } void delegated() { def action = { helper() }; } private void helper() {} private void audit() {} }\n",
    )]);
    for (name, member) in [
        ("helper", "p::Worker::helper"),
        ("this.audit", "p::Worker::audit"),
    ] {
        target(
            &facts,
            ("Worker.groovy", "p::Worker::run", name),
            ("Worker.groovy", member, "native-current-class-call"),
        );
    }
    let owner = capability_symbol(&facts, "Worker.groovy", "p::Worker::delegated");
    let helper = capability_symbol(&facts, "Worker.groovy", "p::Worker::helper");
    assert!(!facts.references().iter().any(|reference| {
        reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
            && reference.target_symbol_id.as_ref() == Some(&helper.symbol_id)
    }));
}

#[test]
fn apex_and_dart_static_calls_bind_a_unique_static_method() {
    for (path, source) in [
        (
            "Worker.cls",
            "public class Worker { public static void create() {} public void instance() {} } public class Use { void run() { Worker.create(); Worker.instance(); } }",
        ),
        (
            "worker.dart",
            "class Worker { static void create() {} void instance() {} } void run() { Worker.create(); Worker.instance(); }",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let caller = if path.ends_with("cls") {
            "Use::run"
        } else {
            "run"
        };
        target(
            &facts,
            (path, caller, "Worker.create"),
            (path, "Worker::create", "native-qualified-member"),
        );
        let owner = capability_symbol(&facts, path, caller);
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("Worker.instance", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none());
    }
}

#[test]
fn added_static_rules_abstain_for_value_receivers_and_intermediate_members() {
    for (path, source, owner) in [
        (
            "Worker.cls",
            "public class Worker { public static void create() {} } public class Use { void run(Object Worker) { Worker.create(); Worker.child.create(); } }",
            "Use::run",
        ),
        (
            "worker.dart",
            "class Worker { static void create() {} } void run(dynamic Worker) { Worker.create(); Worker.child.create(); }",
            "run",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let owner = capability_symbol(&facts, path, owner);
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("Worker.create", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        let method = capability_symbol(&facts, path, "Worker::create");
        assert!(!facts.references().iter().any(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference.target_symbol_id.as_ref() == Some(&method.symbol_id)
        }));
    }
}

#[test]
fn scala_typed_receivers_are_not_proven_nominal_receivers() {
    let source = "object Worker { def create(): Unit = {} }\nclass Use {\n def clear(): Unit = { Worker.create() }\n def run(Worker: AnyRef): Unit = { Worker.create() }\n}\n";
    let extracted = extract_fixture("Worker.scala", source);
    for (owner, expected) in [("Use::clear", true), ("Use::run", false)] {
        let owner = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == owner)
            .unwrap_or_else(|| panic!("missing {owner}: {:?}", extracted.symbols));
        let reference = extracted
            .references
            .iter()
            .find(|reference| {
                reference.owner.as_ref() == Some(&owner.id) && reference.name == "Worker.create"
            })
            .unwrap_or_else(|| panic!("missing Worker.create call"));
        assert_eq!(
            extracted
                .call_scope_sites
                .iter()
                .any(|site| { site.owner == owner.id && site.span == reference.span }),
            expected
        );
    }
    let facts = generation(&[("Worker.scala", source)]);
    let owner = capability_symbol(&facts, "Worker.scala", "Use::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Worker.create", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn dart_factories_can_call_static_members_but_have_no_instance_receiver() {
    let facts = generation(&[(
        "lib/worker.dart",
        "class Worker { factory Worker.make() { helper(); return Worker._(); } factory Worker.valid() { shared(); return Worker._(); } Worker._(); void helper() {} static void shared() {} }",
    )]);
    target(
        &facts,
        ("lib/worker.dart", "Worker::valid", "shared"),
        (
            "lib/worker.dart",
            "Worker::shared",
            "native-current-class-call",
        ),
    );
    let owner = capability_symbol(&facts, "lib/worker.dart", "Worker::make");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn dart_normalized_computed_receivers_do_not_prove_current_class_calls() {
    let facts = generation(&[(
        "lib/worker.dart",
        "class Other { void helper() {} } class Worker { void run() { unknown().helper(); } void helper() {} } dynamic unknown() => null;",
    )]);
    let owner = capability_symbol(&facts, "lib/worker.dart", "Worker::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn dart_explicit_this_keeps_instance_dispatch_and_does_not_invoke_constructors() {
    let facts = generation(&[(
        "lib/worker.dart",
        "class Worker { Worker(); Worker.named(); void run() { this.shared(); this.named(); Worker(); } factory Worker.make() { this.helper(); return Worker.named(); } void helper() {} static void shared() {} }",
    )]);
    for (owner, name) in [
        ("Worker::run", "shared"),
        ("Worker::run", "named"),
        ("Worker::make", "helper"),
    ] {
        let owner = capability_symbol(&facts, "lib/worker.dart", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
    let owner = capability_symbol(&facts, "lib/worker.dart", "Worker::run");
    let constructor = capability_symbol(&facts, "lib/worker.dart", "Worker::Worker");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Worker", ReferenceKind::Calls);
    assert_ne!(
        reference.target_symbol_id.as_ref(),
        Some(&constructor.symbol_id)
    );
}

#[test]
fn inherited_calls_follow_declared_bases_and_abstain_for_unknown_or_private_members() {
    for (path, source) in [
        (
            "worker.dart",
            "class Base { void helper() {} } class Child extends Base { void run() { helper(); } } class Unknown extends Absent { void run() { helper(); } }",
        ),
        (
            "Worker.cls",
            "public class Base { public void helper() {} private void hidden() {} } public class Child extends Base implements External { void run() { helper(); hidden(); } } public class Unknown extends Absent { void run() { helper(); } }",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        target(
            &facts,
            (path, "Child::run", "helper"),
            (path, "Base::helper", "native-inherited-receiver-type"),
        );
        let owner = capability_symbol(&facts, path, "Unknown::run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        if path.ends_with("cls") {
            let owner = capability_symbol(&facts, path, "Child::run");
            let reference =
                CapabilityReferenceQuery::new(&facts, owner).named("hidden", ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        }
    }
}

#[test]
fn class_field_arrows_keep_lexical_this_and_regular_functions_do_not() {
    let facts = generation(&[(
        "src/worker.ts",
        "export class Worker { onClick = () => { this.helper(); }; dynamic = function() { this.helper(); }; helper() {} } class Other { helper() {} }",
    )]);
    target(
        &facts,
        ("src/worker.ts", "Worker::onClick", "this.helper"),
        (
            "src/worker.ts",
            "Worker::helper",
            "native-current-class-call",
        ),
    );
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::dynamic");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn ruby_includes_follow_only_declared_mixins_and_keep_competing_targets_unresolved() {
    let facts = generation(&[(
        "worker.rb",
        "module Audit\n def log_event\n end\nend\nmodule Other\n def log_event\n end\nend\nclass Base\n def save\n end\nend\nclass Worker < Base\n include Audit\n def run\n log_event\n save\n end\nend\nclass Ambiguous\n include Audit\n include Other\n def run\n log_event\n end\nend\n",
    )]);
    for (name, method) in [("log_event", "Audit::log_event"), ("save", "Base::save")] {
        target(
            &facts,
            ("worker.rb", "Worker::run", name),
            ("worker.rb", method, "native-inherited-receiver-type"),
        );
    }
    let owner = capability_symbol(&facts, "worker.rb", "Ambiguous::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("log_event", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn ruby_bare_instance_calls_do_not_invoke_singleton_methods() {
    let facts = generation(&[(
        "worker.rb",
        "class Worker\n def self.helper\n end\n def run\n helper\n end\nend\nclass Instance\n def helper\n end\n def run\n helper\n end\nend\n",
    )]);
    target(
        &facts,
        ("worker.rb", "Instance::run", "helper"),
        ("worker.rb", "Instance::helper", "native-exact-lexical"),
    );
    let owner = capability_symbol(&facts, "worker.rb", "Worker::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn dart_redirects_target_exact_constructors_and_abstain_for_missing_members() {
    let facts = generation(&[(
        "lib/model.dart",
        "class Base { Base(int value); Base.named(int value); } class Child extends Base { Child(): super(1); Child.named(): super.named(2); Child.other(): this.named(); Child.missing(): super.absent(); } extension type Id(int value) { Id.zero(): this(0); }",
    )]);
    for (owner, name, constructor) in [
        ("Child::Child", "super", "Base::Base"),
        ("Child::named", "super.named", "Base::named"),
        ("Child::other", "this.named", "Child::named"),
        ("Id::zero", "this", "Id::Id"),
    ] {
        target(
            &facts,
            ("lib/model.dart", owner, name),
            (
                "lib/model.dart",
                constructor,
                "native-dart-constructor-redirect",
            ),
        );
    }
    let owner = capability_symbol(&facts, "lib/model.dart", "Child::missing");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("super.absent", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn arkts_default_public_methods_resolve_and_partial_script_views_abstain() {
    for (path, source) in [
        (
            "worker.ets",
            "class Ticker { tick() {} private hidden() {} } function run() { new Ticker().tick(); new Ticker().hidden(); }",
        ),
        (
            "Worker.vue",
            "<script>class Ticker { tick() {} } function run() { new Ticker().tick(); new Ticker().absent(); }</script>",
        ),
        (
            "Worker.svelte",
            "<script>class Ticker { tick() {} } function run() { new Ticker().tick(); new Ticker().absent(); }</script>",
        ),
        (
            "Worker.astro",
            "---\nclass Ticker { tick() {} } function run() { new Ticker().tick(); new Ticker().absent(); }\n---\n<div />",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let owner = capability_symbol(&facts, path, "run");
        if path.ends_with("ets") {
            target(
                &facts,
                (path, "run", "tick"),
                (path, "Ticker::tick", "native-dynamic-dispatch"),
            );
        } else {
            let reference =
                CapabilityReferenceQuery::new(&facts, owner).named("tick", ReferenceKind::Calls);
            assert!(
                reference.target_symbol_id.is_none(),
                "{path}: {reference:?}"
            );
        }
        let name = if path.ends_with("ets") {
            "hidden"
        } else {
            "absent"
        };
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn csharp_dotted_namespaces_follow_root_usings_and_keep_ambiguous_types_unresolved() {
    let facts = generation(&[
        (
            "Models.cs",
            "namespace Shop.Models { public class Order {} } namespace Other.Models { public class Order {} }",
        ),
        (
            "Service.cs",
            "using Shop.Models; namespace Shop.Services { public class Service { public Order Load() { return new Order(); } } }",
        ),
        (
            "Ambiguous.cs",
            "using Shop.Models; using Other.Models; namespace Shop.Services { public class Ambiguous { public Order Load() { return null; } } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "Service.cs", "Shop.Services::Service::Load");
    let expected = capability_symbol(&facts, "Models.cs", "Shop.Models::Order");
    for kind in [ReferenceKind::Returns, ReferenceKind::Instantiates] {
        let reference = CapabilityReferenceQuery::new(&facts, owner).named("Order", kind);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&expected.symbol_id),
            "{reference:?}"
        );
        assert_eq!(
            reference.resolution_provenance,
            "native-csharp-namespace-type"
        );
    }
    let owner = capability_symbol(&facts, "Ambiguous.cs", "Shop.Services::Ambiguous::Load");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Order", ReferenceKind::Returns);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn jvm_external_wildcards_keep_same_package_targets_and_ambiguous_packages_abstain() {
    let facts = generation(&[
        ("Own.java", "package p; public class Helper {}"),
        ("Q.java", "package q; public class Helper {}"),
        ("R.java", "package r; public class Helper {}"),
        (
            "Use.java",
            "package p; import java.util.*; import q.*; public class Use { Helper value; }",
        ),
        (
            "Ambiguous.java",
            "package client; import q.*; import r.*; public class Ambiguous { Helper value; }",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.java", "p::Use::value");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Helper", ReferenceKind::TypeOf);
    let expected = capability_symbol(&facts, "Own.java", "p::Helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&expected.symbol_id)
    );
    assert_eq!(reference.resolution_provenance, "native-jvm-package");
    let owner = capability_symbol(&facts, "Ambiguous.java", "client::Ambiguous::value");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Helper", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn jvm_explicit_imports_and_kotlin_aliases_keep_exact_targets_and_private_types_hidden() {
    let facts = generation(&[
        (
            "Service.java",
            "package service; public class Converter {} class Hidden {}",
        ),
        ("Dao.java", "package dao; public class Converter {}"),
        (
            "Use.kt",
            "package client\nimport service.Converter as Engine\nimport service.Hidden\nclass Use { fun run() = Engine(); fun hidden() = Hidden() }",
        ),
    ]);
    let owner = capability_symbol(&facts, "Use.kt", "client::Use::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Engine", ReferenceKind::Instantiates);
    let expected = capability_symbol(&facts, "Service.java", "service::Converter");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&expected.symbol_id)
    );
    assert_eq!(
        reference.resolution_provenance,
        "native-jvm-explicit-import"
    );
    let owner = capability_symbol(&facts, "Use.kt", "client::Use::hidden");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("Hidden", ReferenceKind::Instantiates);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn go_literal_fields_preserve_typed_targets_and_unknown_keys_abstain() {
    let facts = generation(&[(
        "model.go",
        "package p\ntype Profile struct { Bio string }\ntype User struct { Email string }\nfunc NewUser() *User { p := &Profile{Bio: \"new\"}; _ = p; return &User{Email: \"email\"} }\nfunc Unknown() *User { return &User{Missing: \"unknown\"} }\n",
    )]);
    let owner = capability_symbol(&facts, "model.go", "NewUser");
    for (name, member) in [("Bio", "Profile::Bio"), ("Email", "User::Email")] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::FieldAccess);
        let expected = capability_symbol(&facts, "model.go", member);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&expected.symbol_id)
        );
        assert_eq!(
            reference.resolution_provenance,
            "native-explicit-receiver-type"
        );
    }
    let owner = capability_symbol(&facts, "model.go", "Unknown");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("Missing", ReferenceKind::FieldAccess)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn schema_shape_members_are_contract_references_and_missing_fields_abstain() {
    let facts = generation(&[(
        "schema.ts",
        "import { z } from 'zod'; export const UserSchema = z.object({ name: z.string() }); export function read() { return UserSchema.shape.name; } export function missing() { return UserSchema.shape.absent; }",
    )]);
    let field = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.symbol_kind == "field" && symbol.qualified_name == "UserSchema::name")
        .unwrap_or_else(|| panic!("schema field"));
    let owner = capability_symbol(&facts, "schema.ts", "read");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("name", ReferenceKind::References);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&field.symbol_id));
    assert_eq!(reference.resolution_provenance, "native-exact-same-file");
    let owner = capability_symbol(&facts, "schema.ts", "missing");
    assert!(
        !facts
            .references()
            .iter()
            .any(
                |reference| reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && reference.target_symbol_id.as_ref() == Some(&field.symbol_id)
            )
    );
}

#[test]
fn inherited_value_receivers_and_unknown_ancestors_withhold_nominal_static_targets() {
    let facts = generation(&[(
        "worker.dart",
        "class Factory { static void create() {} } class Other { void create() {} } class Base { final Factory = Other(); } class Use extends Base { void run() { Factory.create(); } } class External extends Unknown { void run() { Factory.create(); } } class Clean {} class Clear extends Clean { void run() { Factory.create(); } }",
    )]);
    target(
        &facts,
        ("worker.dart", "Clear::run", "Factory.create"),
        ("worker.dart", "Factory::create", "native-qualified-member"),
    );
    for owner in ["Use::run", "External::run"] {
        let owner = capability_symbol(&facts, "worker.dart", owner);
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("Factory.create", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn rebound_javascript_constructor_names_withhold_only_the_added_local_path() {
    for (path, write) in [
        ("worker.js", "A = B;"),
        ("worker.js", "(A) = B;"),
        ("worker.js", "(A)++;"),
        ("worker.js", "([A] = [B]);"),
        ("worker.js", "({member: A} = {member: B});"),
        ("worker.js", "A++;"),
        ("worker.js", "for (A of [B]) {}"),
        ("worker.js", "eval('A = B');"),
        ("worker.ts", "A! = B;"),
        ("worker.ts", "(A as typeof B) = B;"),
        ("worker.ts", "(<typeof B>A) = B;"),
    ] {
        let source = format!(
            "class A {{ tick() {{}} }} class B {{ tick() {{}} }} {write} function run() {{ new A().tick(); }}"
        );
        let facts = generation(&[(path, &source)]);
        let owner = capability_symbol(&facts, path, "run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("tick", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{write}: {reference:?}"
        );
    }
    let facts = generation(&[(
        "worker.js",
        "class A { tick() {} } class B { tick() {} } function run() { new A().tick(); }",
    )]);
    target(
        &facts,
        ("worker.js", "run", "tick"),
        ("worker.js", "A::tick", "native-dynamic-dispatch"),
    );
}

#[test]
fn ruby_unmodelled_method_changes_fence_inherited_names() {
    for (change, all_names) in [
        ("alias_method :helper, :other", false),
        ("name = :helper; alias_method name, :other", true),
        ("name = :helper; define_method(name) { :other }", true),
        ("alias helper other", false),
        ("undef helper", false),
        ("undef_method :helper", false),
        ("remove_method :helper", false),
        ("define_method(:helper) { :other }", false),
        ("def method_missing(name); :other; end", true),
        ("def respond_to_missing?(name); true; end", true),
    ] {
        let source = format!(
            "class Base\n def helper; :base; end\n def spare; :base; end\nend\nclass Child < Base\n def other; :other; end\n {change}\n def run; helper; spare; end\nend\n"
        );
        let facts = generation(&[("worker.rb", &source)]);
        let owner = capability_symbol(&facts, "worker.rb", "Child::run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{change}: {reference:?}"
        );
        if all_names {
            let reference =
                CapabilityReferenceQuery::new(&facts, owner).named("spare", ReferenceKind::Calls);
            assert!(
                reference.target_symbol_id.is_none(),
                "{change}: {reference:?}"
            );
        } else {
            target(
                &facts,
                ("worker.rb", "Child::run", "spare"),
                ("worker.rb", "Base::spare", "native-inherited-receiver-type"),
            );
        }
    }
}

#[test]
fn ruby_overridden_include_or_extend_does_not_emit_mixin_ancestry() {
    for definition in [
        "def self.include(mod); end",
        "def include(mod); end",
        "def extend(mod); end",
        "name = :include; define_method(name) { |mod| }",
    ] {
        let source = format!(
            "module Audit\n def log_event; end\nend\nclass Worker\n {definition}\n include Audit\n def run; log_event; end\nend\n"
        );
        let extracted = extract_fixture("worker.rb", &source);
        let owner = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Worker")
            .unwrap_or_else(|| panic!("missing Worker"));
        assert!(
            !extracted.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(&owner.id)
                    && reference.kind == ReferenceKind::Inherits
                    && reference.name == "Audit"
            }),
            "{definition}: {:?}",
            extracted.references
        );
        let facts = generation(&[("worker.rb", &source)]);
        let owner = capability_symbol(&facts, "worker.rb", "Worker::run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("log_event", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{definition}: {reference:?}"
        );
    }
}
