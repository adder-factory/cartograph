use super::*;

#[test]
fn default_packages_and_non_public_types_cannot_bind_foreign_namesakes() {
    let facts = generation(&[
        (
            "foreign/Helper.java",
            "package foreign; public class Helper {}",
        ),
        (
            "foreign/Hidden.java",
            "package foreign; private class Hidden {}",
        ),
        (
            "foreign/Internal.java",
            "package foreign; class Internal {}",
        ),
        ("default/Use.java", "class Use { Helper value; }"),
        (
            "client/Use.java",
            "package client; import foreign.Hidden; import foreign.Internal; public class Use { Hidden hidden; Internal internal; }",
        ),
        (
            "foreign/path/Helper.kt",
            "class Wrong { fun make() = Helper() }",
        ),
    ]);
    let value = capability_symbol(&facts, "default/Use.java", "Use::value");
    assert!(
        CapabilityReferenceQuery::new(&facts, value)
            .named("Helper", ReferenceKind::TypeOf)
            .target_symbol_id
            .is_none()
    );
    for (field, name) in [("hidden", "Hidden"), ("internal", "Internal")] {
        let owner = capability_symbol(&facts, "client/Use.java", &format!("client::Use::{field}"));
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::TypeOf);
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(reference.resolution_provenance, "native-unresolved-import");
    }
    let wrong = capability_symbol(&facts, "foreign/path/Helper.kt", "Wrong::make");
    assert!(
        CapabilityReferenceQuery::new(&facts, wrong)
            .named("Helper", ReferenceKind::Instantiates)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn array_and_generic_receivers_do_not_name_their_element_types_members() {
    let facts = generation(&[
        (
            "p/Converter.java",
            "package p; public class Converter { public void convert() {} }",
        ),
        (
            "p/Use.java",
            "package p; public class Use { Converter[] values; java.util.List<Converter> list; void run() { values.convert(); list.convert(); } }",
        ),
    ]);
    let caller = capability_symbol(&facts, "p/Use.java", "p::Use::run");
    for name in ["values.convert", "list.convert"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn lexical_type_parameters_shadow_imports_and_package_classes() {
    for (path, source, field, caller) in [
        (
            "client/Use.java",
            "package client; import p.T; public class Use<T> { T value; void run() { value.go(); T.go(); } }",
            "client::Use::value",
            "client::Use::run",
        ),
        (
            "client/Use.kt",
            "package client\nimport p.T\nclass Use<T> { val value: T\nfun run() { value.go(); T.go() } }",
            "client::Use::value",
            "client::Use::run",
        ),
    ] {
        let facts = generation(&[
            (
                "p/T.java",
                "package p; public class T { public static void go() {} }",
            ),
            (path, source),
        ]);
        let field = capability_symbol(&facts, path, field);
        let type_use =
            CapabilityReferenceQuery::new(&facts, field).named("T", ReferenceKind::TypeOf);
        assert!(type_use.target_symbol_id.is_none(), "{type_use:?}");
        let caller = capability_symbol(&facts, path, caller);
        for name in ["value.go", "T.go"] {
            let reference =
                CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        }
    }
}

#[test]
fn nested_import_aliases_do_not_widen_when_the_imported_outer_has_no_member() {
    let facts = generation(&[
        ("p/Outer.kt", "package p\nclass Outer { class Inner }"),
        ("q/Outer.kt", "package q\nclass Outer { class Missing }"),
        (
            "client/Use.kt",
            "package client\nimport p.Outer as Alias\nclass Use { fun ok(x: Alias.Inner) {}\nfun missing(x: Alias.Missing) {} }",
        ),
    ]);
    let ok = capability_symbol(&facts, "client/Use.kt", "client::Use::ok");
    targets(
        CapabilityReferenceQuery::new(&facts, ok).named("Inner", ReferenceKind::TypeOf),
        capability_symbol(&facts, "p/Outer.kt", "p::Outer::Inner"),
        "native-jvm-explicit-import",
    );
    let missing = capability_symbol(&facts, "client/Use.kt", "client::Use::missing");
    let reference =
        CapabilityReferenceQuery::new(&facts, missing).named("Missing", ReferenceKind::TypeOf);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn namespace_function_calls_keep_existing_import_resolution() {
    let facts = generation(&[
        (
            "src/tools.ts",
            "export function create() {} export class Builder { static create() {} }",
        ),
        (
            "src/use.ts",
            "import * as tools from './tools'; export function run() { tools.create(); tools.Builder.create(); }",
        ),
    ]);
    let caller = capability_symbol(&facts, "src/use.ts", "run");
    targets(
        CapabilityReferenceQuery::new(&facts, caller).named("tools.create", ReferenceKind::Calls),
        capability_symbol(&facts, "src/tools.ts", "create"),
        "native-import-binding",
    );
    targets(
        CapabilityReferenceQuery::new(&facts, caller)
            .named("tools.Builder.create", ReferenceKind::Calls),
        capability_symbol(&facts, "src/tools.ts", "Builder::create"),
        "native-qualified-member",
    );
}
