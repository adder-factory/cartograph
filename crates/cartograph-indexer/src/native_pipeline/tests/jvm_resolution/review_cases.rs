use super::*;

#[test]
fn field_receiver_types_use_the_declaration_scope() {
    let facts = generation(&[
        (
            "p/Converter.java",
            "package p; public class Converter { public void go() {} }",
        ),
        (
            "p/Use.java",
            "package p; class Use { Converter value; Unknown missing; void run() { class Converter { public void go() {} } class Unknown { public void go() {} } value.go(); missing.go(); } }",
        ),
    ]);
    let run = capability_symbol(&facts, "p/Use.java", "p::Use::run");
    targets(
        CapabilityReferenceQuery::new(&facts, run).named("value.go", ReferenceKind::Calls),
        capability_symbol(&facts, "p/Converter.java", "p::Converter::go"),
        "native-dynamic-dispatch",
    );
    let missing =
        CapabilityReferenceQuery::new(&facts, run).named("missing.go", ReferenceKind::Calls);
    assert!(missing.target_symbol_id.is_none(), "{missing:?}");
}

#[test]
fn distinct_nominal_overloads_cannot_collapse_to_the_only_body() {
    for (path, source) in [
        (
            "Builder.java",
            "class Builder { public static void go() {} public static native void go(int value); public static void unique() {} void run() { Builder.go(1); Builder.unique(); } }",
        ),
        (
            "Builder.cs",
            "class Builder { public static void go() {} public static extern void go(int value); public static void unique() {} void run() { Builder.go(1); Builder.unique(); } }",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let run = capability_symbol(&facts, path, "Builder::run");
        let overloaded =
            CapabilityReferenceQuery::new(&facts, run).named("Builder.go", ReferenceKind::Calls);
        assert!(overloaded.target_symbol_id.is_none(), "{overloaded:?}");
        targets(
            CapabilityReferenceQuery::new(&facts, run)
                .named("Builder.unique", ReferenceKind::Calls),
            capability_symbol(&facts, path, "Builder::unique"),
            "native-qualified-member",
        );
    }
}

#[test]
fn abstract_instance_overloads_remain_ambiguous_for_typed_receivers() {
    for (path, source) in [
        (
            "Builder.java",
            "abstract class Builder { Builder value; public void go() {} public abstract void go(int value); public void unique() {} void run() { value.go(1); value.unique(); } }",
        ),
        (
            "Builder.kt",
            "abstract class Builder { val value: Builder\nfun go() {}\nabstract fun go(value: Int)\nfun unique() {}\nfun run() { value.go(1); value.unique() } }",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let run = capability_symbol(&facts, path, "Builder::run");
        let overloaded =
            CapabilityReferenceQuery::new(&facts, run).named("value.go", ReferenceKind::Calls);
        assert!(overloaded.target_symbol_id.is_none(), "{overloaded:?}");
        targets(
            CapabilityReferenceQuery::new(&facts, run).named("value.unique", ReferenceKind::Calls),
            capability_symbol(&facts, path, "Builder::unique"),
            "native-dynamic-dispatch",
        );
    }
}

#[test]
fn nested_type_imports_require_every_nominal_ancestor_to_be_accessible() {
    let facts = generation(&[
        (
            "p/Types.java",
            "package p; class Hidden { public static class Inner {} } public class Visible { public static class Inner {} private static class Hidden { public static class Deep {} } }",
        ),
        (
            "q/Use.java",
            "package q; import p.Hidden.Inner; import p.Visible.Hidden.Deep; public class Use { Inner hidden; Deep deep; p.Visible.Inner visible; }",
        ),
        (
            "q/Use.kt",
            "package q\nimport p.Hidden as Alias\nimport p.Visible as PublicAlias\nclass KUse { val hidden: Alias.Inner\nval visible: PublicAlias.Inner }",
        ),
    ]);
    for (path, owner, name) in [
        ("q/Use.java", "q::Use::hidden", "Inner"),
        ("q/Use.java", "q::Use::deep", "Deep"),
        ("q/Use.kt", "q::KUse::hidden", "Inner"),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::TypeOf);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
    for (path, owner, name, provenance) in [
        (
            "q/Use.java",
            "q::Use::visible",
            "p.Visible.Inner",
            "native-jvm-qualified-type",
        ),
        (
            "q/Use.kt",
            "q::KUse::visible",
            "Inner",
            "native-jvm-explicit-import",
        ),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        targets(
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::TypeOf),
            capability_symbol(&facts, "p/Types.java", "p::Visible::Inner"),
            provenance,
        );
    }
}

#[test]
fn method_local_types_are_lexical_and_cannot_be_imported_as_package_types() {
    let facts = generation(&[
        (
            "p/Use.java",
            "package p; public class Use { public void run() { class Local {} Local value; p.Use.run.Local malformed; } }",
        ),
        (
            "p/Imported.java",
            "package p; import p.Use.run.Local; public class Imported { Local value; }",
        ),
        (
            "p/Qualified.kt",
            "package p\nclass Qualified { val value: p.Use.run.Local }",
        ),
    ]);
    for (path, owner, name) in [
        ("p/Imported.java", "p::Imported::value", "Local"),
        ("p/Qualified.kt", "p::Qualified::value", "Local"),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::TypeOf);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
    let value = capability_symbol(&facts, "p/Use.java", "p::Use::run::value");
    targets(
        CapabilityReferenceQuery::new(&facts, value).named("Local", ReferenceKind::TypeOf),
        capability_symbol(&facts, "p/Use.java", "p::Use::run::Local"),
        "native-exact-lexical",
    );
    let malformed = capability_symbol(&facts, "p/Use.java", "p::Use::run::malformed");
    let reference = CapabilityReferenceQuery::new(&facts, malformed)
        .named("p.Use.run.Local", ReferenceKind::TypeOf);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn kotlin_type_aliases_do_not_create_java_visible_nominal_types() {
    let facts = generation(&[
        (
            "p/Alias.kt",
            "package p\nclass Actual\ntypealias Alias = Actual",
        ),
        (
            "q/Use.java",
            "package q; import p.Alias; public class Use { Alias value; }",
        ),
        (
            "q/Use.kt",
            "package q\nimport p.Alias\nclass KUse { val value: Alias }",
        ),
    ]);
    let field = capability_symbol(&facts, "q/Use.java", "q::Use::value");
    let reference =
        CapabilityReferenceQuery::new(&facts, field).named("Alias", ReferenceKind::TypeOf);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    let field = capability_symbol(&facts, "q/Use.kt", "q::KUse::value");
    targets(
        CapabilityReferenceQuery::new(&facts, field).named("Alias", ReferenceKind::TypeOf),
        capability_symbol(&facts, "p/Alias.kt", "p::Alias"),
        "native-jvm-explicit-import",
    );
}
