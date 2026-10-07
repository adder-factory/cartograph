use super::*;

#[test]
fn java_local_types_require_declaration_order_and_containing_blocks() {
    let facts = generation(&[(
        "Use.java",
        "class T {} class Use { void run() { T first; class T {} T later; } void block() { { class T {} T inside; } T outside; } }",
    )]);
    let top = capability_symbol(&facts, "Use.java", "T");
    for (owner, target, provenance) in [
        ("Use::run::first", top, "native-exact-same-file"),
        ("Use::block::outside", top, "native-exact-same-file"),
        (
            "Use::run::later",
            capability_symbol(&facts, "Use.java", "Use::run::T"),
            "native-exact-lexical",
        ),
        (
            "Use::block::inside",
            capability_symbol(&facts, "Use.java", "Use::block::T"),
            "native-exact-lexical",
        ),
    ] {
        targets(
            CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "Use.java", owner))
                .named("T", ReferenceKind::TypeOf),
            target,
            provenance,
        );
    }
}

#[test]
fn java_enhanced_for_receivers_abstain_inside_the_binding_scope() {
    let facts = generation(&[(
        "Use.java",
        "class Builder { public static void create() {} } class Other { public void create() {} } class Use { void run(Other[] values) { for (Other Builder : values) { Builder.create(); } } void direct() { Builder.create(); } }",
    )]);
    let run = capability_symbol(&facts, "Use.java", "Use::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, run).named("Builder.create", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved");
    targets(
        CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "Use.java", "Use::direct"))
            .named("Builder.create", ReferenceKind::Calls),
        capability_symbol(&facts, "Use.java", "Builder::create"),
        "native-qualified-member",
    );
}

#[test]
fn local_receiver_types_without_signature_evidence_abstain() {
    let facts = generation(&[(
        "Use.java",
        "class T { public void go() {} } class Use { void run() { T value = null; class T { public void go() {} } value.go(); } }",
    )]);
    let reference =
        CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "Use.java", "Use::run"))
            .named("value.go", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved");
}

#[test]
fn nominal_receivers_abstain_when_ancestry_can_supply_a_field() {
    for ancestor in ["Base", "External"] {
        let source = format!(
            "class Builder {{ public static void create() {{}} }} class Other {{ public void create() {{}} }} class Base {{ Other Builder; }} class Use extends {ancestor} {{ void run() {{ Builder.create(); }} }} class Plain {{ void run() {{ Builder.create(); }} }}"
        );
        let facts = generation(&[("Use.java", &source)]);
        let reference = CapabilityReferenceQuery::new(
            &facts,
            capability_symbol(&facts, "Use.java", "Use::run"),
        )
        .named("Builder.create", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        assert_eq!(reference.resolution_provenance, "native-unresolved");
        targets(
            CapabilityReferenceQuery::new(
                &facts,
                capability_symbol(&facts, "Use.java", "Plain::run"),
            )
            .named("Builder.create", ReferenceKind::Calls),
            capability_symbol(&facts, "Use.java", "Builder::create"),
            "native-qualified-member",
        );
    }
}

#[test]
fn named_imports_cannot_drop_intermediate_receiver_members() {
    let facts = generation(&[
        (
            "builder.ts",
            "export class Child { static create() {} } export class Builder { static child = Child; static create() {} }",
        ),
        (
            "use.ts",
            "import { Builder } from './builder'; export function run() { Builder.child.create(); } export function direct() { Builder.create(); }",
        ),
    ]);
    let reference =
        CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "use.ts", "run"))
            .named("Builder.child.create", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-dynamic-unresolved");
    targets(
        CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "use.ts", "direct"))
            .named("Builder.create", ReferenceKind::Calls),
        capability_symbol(&facts, "builder.ts", "Builder::create"),
        "native-import-binding",
    );
}

#[test]
fn kotlin_instance_invoke_methods_do_not_hide_constructors() {
    let facts = generation(&[(
        "Maker.kt",
        "class Maker { fun invoke() {} }\nfun make() = Maker()\nclass OperatorMaker() { operator fun invoke() {} }\nfun operatorMake() = OperatorMaker()",
    )]);
    for (caller, name) in [("make", "Maker"), ("operatorMake", "OperatorMaker")] {
        targets(
            CapabilityReferenceQuery::new(&facts, capability_symbol(&facts, "Maker.kt", caller))
                .named(name, ReferenceKind::Instantiates),
            capability_symbol(&facts, "Maker.kt", name),
            "native-exact-same-file",
        );
    }
}

#[test]
fn kotlin_companion_factories_do_not_treat_constructors_as_value_shadows() {
    let facts = generation(&[(
        "model/User.kt",
        r#"package com.acme.app.model
typealias UserId = String
abstract class Entity(open val id: UserId)
interface Auditable
data class User(override val id: UserId, var name: String): Entity(id), Auditable {
    companion object {
        fun create(id: UserId): User = User(id, "anon")
    }
}
class Shadowed(val id: UserId) {
    companion object {
        fun Shadowed(id: UserId) {}
        fun create(id: UserId) = Shadowed(id)
    }
}
"#,
    )]);
    let path = "model/User.kt";
    let caller = capability_symbol(&facts, path, "com.acme.app.model::User::create");
    let target = capability_symbol(&facts, path, "com.acme.app.model::User");
    targets(
        CapabilityReferenceQuery::new(&facts, caller).named("User", ReferenceKind::Instantiates),
        target,
        "native-exact-lexical",
    );
    assert!(facts.edges().iter().any(|edge| {
        edge.source_symbol_id == caller.symbol_id
            && edge.target_symbol_id == target.symbol_id
            && edge.kind == EdgeKind::Instantiates
    }));
    let shadowed = capability_symbol(&facts, path, "com.acme.app.model::Shadowed::create");
    let reference = CapabilityReferenceQuery::new(&facts, shadowed)
        .named("Shadowed", ReferenceKind::Instantiates);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved");
}

#[test]
fn wildcard_package_names_preserve_callable_ambiguity() {
    let facts = generation(&[
        ("a/Builder.kt", "package a\nclass Builder(value: Int)"),
        ("b/Builder.kt", "package b\nfun Builder(value: Int) {}"),
        ("Use.kt", "import a.*\nimport b.*\nfun make() = Builder(1)"),
    ]);
    let caller = capability_symbol(&facts, "Use.kt", "make");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("Builder", ReferenceKind::Instantiates);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved-import");
}
