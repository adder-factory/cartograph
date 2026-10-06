use super::*;

#[test]
fn same_file_top_level_types_do_not_bypass_nearer_receiver_bindings() {
    for (path, source, owner, target) in [
        (
            "Use.java",
            "class Builder { public static void create() {} } class Use { void run() { class Builder { public static void create() {} } Builder.create(); } }",
            "Use::run",
            "Use::run::Builder::create",
        ),
        (
            "Use.cs",
            "class Builder { public static void create() {} } class Use { public class Builder { public static void create() {} } void run() { Builder.create(); } }",
            "Use::run",
            "Use::Builder::create",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let caller = capability_symbol(&facts, path, owner);
        targets(
            CapabilityReferenceQuery::new(&facts, caller)
                .named("Builder.create", ReferenceKind::Calls),
            capability_symbol(&facts, path, target),
            "native-qualified-member",
        );
    }
}

#[test]
fn javascript_local_receiver_bindings_keep_js_scope_ownership() {
    let facts = generation(&[(
        "Use.ts",
        "class Builder { static create() {} } function run() { class Builder { static create() {} } Builder.create(); } function shadow(Builder: object) { Builder.create(); }",
    )]);
    // Both receivers have JS shadow context. It does not distinguish a local
    // class from an anonymous value binding, so nominal lookup must abstain.
    for name in ["run", "shadow"] {
        let owner = capability_symbol(&facts, "Use.ts", name);
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("Builder.create", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        assert_eq!(reference.resolution_provenance, "native-unresolved");
    }
}

#[test]
fn lexical_heads_of_nested_types_cannot_be_replaced_by_imported_namesakes() {
    let facts = generation(&[
        (
            "q/Outer.java",
            "package q; public class Outer { public static class Inner { public static void create() {} } public static class Missing {} }",
        ),
        (
            "p/Use.java",
            "package p; import q.Outer; class Use { void run() { class Outer { class Inner { public static void create() {} } } Outer.Inner value; Outer.Missing missing; Outer.Inner.create(); } }",
        ),
    ]);
    let value = capability_symbol(&facts, "p/Use.java", "p::Use::run::value");
    targets(
        CapabilityReferenceQuery::new(&facts, value).named("Outer.Inner", ReferenceKind::TypeOf),
        capability_symbol(&facts, "p/Use.java", "p::Use::run::Outer::Inner"),
        "native-jvm-qualified-type",
    );
    let run = capability_symbol(&facts, "p/Use.java", "p::Use::run");
    targets(
        CapabilityReferenceQuery::new(&facts, run)
            .named("Outer.Inner.create", ReferenceKind::Calls),
        capability_symbol(&facts, "p/Use.java", "p::Use::run::Outer::Inner::create"),
        "native-qualified-member",
    );
    let missing = capability_symbol(&facts, "p/Use.java", "p::Use::run::missing");
    let reference = CapabilityReferenceQuery::new(&facts, missing)
        .named("Outer.Missing", ReferenceKind::TypeOf);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn chained_value_receivers_cannot_follow_their_heads_nominal_namesakes() {
    let facts = generation(&[(
        "Use.java",
        "class Outer { public static class Inner { public static void go() {} } } class Builder { public void go() {} } class Other { public Builder Inner; } class Use { Other Outer; void run() { Outer.Inner.go(); } }",
    )]);
    let caller = capability_symbol(&facts, "Use.java", "Use::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("Outer.Inner.go", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}
