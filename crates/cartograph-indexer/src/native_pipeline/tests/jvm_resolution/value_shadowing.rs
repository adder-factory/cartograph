use super::*;

#[test]
fn managed_value_parameters_cannot_be_resolved_as_static_type_receivers() {
    for (path, source, target_path, target_source, owner, target) in [
        (
            "client/Use.java",
            "package client; import q.Builder; class Other extends RuntimeException { public void create() {} } class Use { void direct(Other Builder) { Builder.create(); } void lambda() { java.util.function.Consumer<Other> consumer = Builder -> Builder.create(); } void caught() { try {} catch (Other Builder) { Builder.create(); } } void exact() { Builder.create(); } }",
            "q/Builder.java",
            "package q; public class Builder { public static void create() {} }",
            "client::Use",
            "q::Builder::create",
        ),
        (
            "client/Use.kt",
            "package client\nimport q.Builder\nclass Other: RuntimeException() { fun create() {} }\nclass Use { fun direct(Builder: Other) { Builder.create() }\nfun lambda() { consume { Builder: Other -> Builder.create() } }\nfun caught() { try {} catch (Builder: Other) { Builder.create() } }\nfun exact() { Builder.create() } }",
            "q/Builder.java",
            "package q; public class Builder { public static void create() {} }",
            "client::Use",
            "q::Builder::create",
        ),
        (
            "Use.cs",
            "class Other: System.Exception { public void create() {} } class Use { void direct(Other Builder) { Builder.create(); } void lambda() { System.Action<Other> consumer = Builder => Builder.create(); } void caught() { try {} catch (Other Builder) { Builder.create(); } } void exact() { Builder.create(); } }",
            "Builder.cs",
            "public class Builder { public static void create() {} }",
            "Use",
            "Builder::create",
        ),
    ] {
        let facts = generation(&[(path, source), (target_path, target_source)]);
        for method in ["lambda", "caught"] {
            let caller = capability_symbol(&facts, path, &format!("{owner}::{method}"));
            let reference = CapabilityReferenceQuery::new(&facts, caller)
                .named("Builder.create", ReferenceKind::Calls);
            assert!(
                reference.target_symbol_id.is_none(),
                "{path}: {reference:?}"
            );
        }
        let caller = capability_symbol(&facts, path, &format!("{owner}::direct"));
        let other = if path == "Use.cs" {
            "Other::create"
        } else {
            "client::Other::create"
        };
        let direct = CapabilityReferenceQuery::new(&facts, caller)
            .named("Builder.create", ReferenceKind::Calls);
        if path == "client/Use.kt" {
            // The lambda/catch grammar recovery makes this file partial; optional
            // receiver evidence must abstain without changing base resolution.
            assert!(direct.target_symbol_id.is_none(), "{direct:?}");
        } else {
            targets(
                direct,
                capability_symbol(&facts, path, other),
                "native-explicit-receiver-type",
            );
        }
        let caller = capability_symbol(&facts, path, &format!("{owner}::exact"));
        targets(
            CapabilityReferenceQuery::new(&facts, caller)
                .named("Builder.create", ReferenceKind::Calls),
            capability_symbol(&facts, target_path, target),
            "native-qualified-member",
        );
    }
}

#[test]
fn csharp_generic_receivers_do_not_bind_same_named_nominal_types() {
    let facts = generation(&[(
        "Use.cs",
        "class T { public static void create() {} } class Use<T> { void run() { T.create(); } } class Ordinary { void run() { T.create(); } }",
    )]);
    let caller = capability_symbol(&facts, "Use.cs", "Use::run");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("T.create", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    let caller = capability_symbol(&facts, "Use.cs", "Ordinary::run");
    targets(
        CapabilityReferenceQuery::new(&facts, caller).named("T.create", ReferenceKind::Calls),
        capability_symbol(&facts, "Use.cs", "T::create"),
        "native-qualified-member",
    );
}

#[test]
fn kotlin_uppercase_callable_values_do_not_name_imported_constructor_types() {
    let facts = generation(&[
        ("q/Builder.java", "package q; public class Builder {}"),
        (
            "client/Use.kt",
            "package client\nimport q.Builder\nclass Use { fun parameter(Builder: () -> Unit) { Builder() }\nfun binding() { val Builder = {}; Builder() }\nfun exact() { Builder() } }",
        ),
    ]);
    for method in ["parameter", "binding"] {
        let caller = capability_symbol(&facts, "client/Use.kt", &format!("client::Use::{method}"));
        let reference = CapabilityReferenceQuery::new(&facts, caller)
            .named("Builder", ReferenceKind::Instantiates);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
    let caller = capability_symbol(&facts, "client/Use.kt", "client::Use::exact");
    targets(
        CapabilityReferenceQuery::new(&facts, caller).named("Builder", ReferenceKind::Instantiates),
        capability_symbol(&facts, "q/Builder.java", "q::Builder"),
        "native-jvm-explicit-import",
    );
}

#[test]
fn kotlin_constructor_shaped_calls_with_callable_competitors_abstain() {
    let facts = generation(&[
        (
            "q/Builder.kt",
            "package q\nclass Builder\nfun Builder(value: Int) {}",
        ),
        (
            "q/Factory.java",
            "package q; public class Factory { public static class Made {} public static void Made(int value) {} }",
        ),
        (
            "client/Use.kt",
            "package client\nimport q.Builder\nimport q.Factory\nclass Use { fun function() { Builder(1) }\nfun method() { Factory.Made(1) } }",
        ),
    ]);
    for (method, name) in [("function", "Builder"), ("method", "Factory.Made")] {
        let caller = capability_symbol(&facts, "client/Use.kt", &format!("client::Use::{method}"));
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Instantiates);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn kotlin_operator_invoke_does_not_prove_a_constructor_call() {
    let facts = generation(&[
        (
            "q/Types.kt",
            r"package q
object Builder {
    operator fun invoke(value: Int) {}
}
class Factory() {
    companion object {
        operator fun invoke(value: Int) {}
    }
}
class NamedFactory() {
    companion object Named {
        operator fun invoke(value: Int) {}
    }
}
class Outer() {
    object Made {
        operator fun invoke(value: Int) {}
    }
}
class Plain(val value: Int)
fun local() { Builder(1) }
",
        ),
        (
            "client/Use.kt",
            "package client\nimport q.Builder\nimport q.Factory\nimport q.NamedFactory\nimport q.Outer\nimport q.Plain\nclass Use { fun singleton() { Builder(1) }\nfun companion() { Factory(1) }\nfun namedCompanion() { NamedFactory(1) }\nfun nested() { Outer.Made(1) }\nfun constructor() { Plain(1) } }",
        ),
    ]);
    for qualified in [
        "q::Builder",
        "q::Factory",
        "q::NamedFactory",
        "q::Outer::Made",
    ] {
        assert_eq!(
            capability_symbol(&facts, "q/Types.kt", qualified).symbol_kind,
            "class"
        );
    }
    for (method, name) in [
        ("singleton", "Builder"),
        ("companion", "Factory"),
        ("namedCompanion", "NamedFactory"),
        ("nested", "Outer.Made"),
    ] {
        let caller = capability_symbol(&facts, "client/Use.kt", &format!("client::Use::{method}"));
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Instantiates);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        assert_eq!(
            reference.resolution_provenance, "native-unresolved-import",
            "{method}: {reference:?}"
        );
    }
    let local = capability_symbol(&facts, "q/Types.kt", "q::local");
    let reference =
        CapabilityReferenceQuery::new(&facts, local).named("Builder", ReferenceKind::Instantiates);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved");
    let caller = capability_symbol(&facts, "client/Use.kt", "client::Use::constructor");
    targets(
        CapabilityReferenceQuery::new(&facts, caller).named("Plain", ReferenceKind::Instantiates),
        capability_symbol(&facts, "q/Types.kt", "q::Plain"),
        "native-jvm-explicit-import",
    );
}

#[test]
fn kotlin_inherited_invocation_and_aliases_cannot_prove_constructors() {
    let facts = generation(&[
        (
            "q/Types.kt",
            "package q\nopen class Base { operator fun invoke(value: Int) {} }\nobject Inherited : Base()\nclass InheritedFactory { companion object : Base() }\nclass Plain\ntypealias Alias = Plain\nfun local() { Alias() }",
        ),
        (
            "client/Use.kt",
            "package client\nimport q.Inherited\nimport q.InheritedFactory\nimport q.Alias\nclass Use { val type: Alias\nfun singleton() { Inherited(1) }\nfun companion() { InheritedFactory(1) }\nfun alias() { Alias() } }",
        ),
    ]);
    for (qualified, kind) in [
        ("q::Inherited", "class"),
        ("q::InheritedFactory", "class"),
        ("q::Alias", "type_alias"),
    ] {
        assert_eq!(
            capability_symbol(&facts, "q/Types.kt", qualified).symbol_kind,
            kind
        );
    }
    for (method, name) in [
        ("singleton", "Inherited"),
        ("companion", "InheritedFactory"),
        ("alias", "Alias"),
    ] {
        let caller = capability_symbol(&facts, "client/Use.kt", &format!("client::Use::{method}"));
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Instantiates);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        assert_eq!(
            reference.resolution_provenance, "native-unresolved-import",
            "{method}: {reference:?}"
        );
    }
    let local = capability_symbol(&facts, "q/Types.kt", "q::local");
    let reference =
        CapabilityReferenceQuery::new(&facts, local).named("Alias", ReferenceKind::Instantiates);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_eq!(reference.resolution_provenance, "native-unresolved");
    let field = capability_symbol(&facts, "client/Use.kt", "client::Use::type");
    targets(
        CapabilityReferenceQuery::new(&facts, field).named("Alias", ReferenceKind::TypeOf),
        capability_symbol(&facts, "q/Types.kt", "q::Alias"),
        "native-jvm-explicit-import",
    );
}
