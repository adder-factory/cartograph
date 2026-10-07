//! JVM package/import resolution, nominal member calls, and inheritance edges.

mod bounds;
mod corpora;
mod repairs;
mod review_cases;
mod scopes;
mod value_shadowing;

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, ReferenceInput, ReferenceKind,
    SymbolInput, build_capability_generation, capability_symbol,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    forward
}

fn targets(reference: &ReferenceInput, target: &SymbolInput, provenance: &str) {
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{} -> {}: {reference:?}",
        reference.reference_name,
        target.qualified_name
    );
    assert_eq!(reference.resolution_provenance, provenance);
}

#[test]
fn jvm_explicit_imports_and_aliases_disambiguate_declared_packages() {
    let facts = generation(&[
        (
            "generated/Service.java",
            "package service; public class Converter { public String convert(String value) { return value; } }",
        ),
        (
            "generated/Dao.java",
            "package dao; public class Converter { public String convert(String value) { return value; } }",
        ),
        (
            "client/Use.java",
            "package client; import service.Converter; public class Use { private Converter converter; public void run() { converter.convert(null); } }",
        ),
        (
            "client/Use.kt",
            "package client\nimport service.Converter as ServiceConverter\nclass KUse { fun make(): ServiceConverter = ServiceConverter() }",
        ),
    ]);
    let class = capability_symbol(&facts, "generated/Service.java", "service::Converter");
    let field = capability_symbol(&facts, "client/Use.java", "client::Use::converter");
    targets(
        CapabilityReferenceQuery::new(&facts, field).named("Converter", ReferenceKind::TypeOf),
        class,
        "native-jvm-explicit-import",
    );
    let make = capability_symbol(&facts, "client/Use.kt", "client::KUse::make");
    targets(
        CapabilityReferenceQuery::new(&facts, make)
            .named("ServiceConverter", ReferenceKind::Instantiates),
        class,
        "native-jvm-explicit-import",
    );
    let run = capability_symbol(&facts, "client/Use.java", "client::Use::run");
    targets(
        CapabilityReferenceQuery::new(&facts, run).named("converter.convert", ReferenceKind::Calls),
        capability_symbol(
            &facts,
            "generated/Service.java",
            "service::Converter::convert",
        ),
        "native-dynamic-dispatch",
    );
}

#[test]
fn jvm_missing_and_ambiguous_imports_never_bind_namesakes() {
    let facts = generation(&[
        ("p/Converter.java", "package p; public class Converter {}"),
        ("a/Converter.java", "package a; public class Converter {}"),
        ("b/Converter.java", "package b; public class Converter {}"),
        (
            "p/Missing.java",
            "package p; import vendor.Converter; public class Missing { Converter value; }",
        ),
        (
            "p/Tied.kt",
            "package p\nimport a.Converter as Pick\nimport b.Converter as Pick\nclass Tied { fun make() = Pick() }",
        ),
    ]);
    let field = capability_symbol(&facts, "p/Missing.java", "p::Missing::value");
    let missing =
        CapabilityReferenceQuery::new(&facts, field).named("Converter", ReferenceKind::TypeOf);
    assert!(missing.target_symbol_id.is_none());
    assert_eq!(missing.resolution_provenance, "native-external-reference");
    let make = capability_symbol(&facts, "p/Tied.kt", "p::Tied::make");
    let tied =
        CapabilityReferenceQuery::new(&facts, make).named("Pick", ReferenceKind::Instantiates);
    assert!(tied.target_symbol_id.is_none());
    assert_eq!(tied.resolution_provenance, "native-unresolved-import");
}

#[test]
fn jvm_java_and_kotlin_types_share_packages_across_source_roots() {
    let facts = generation(&[
        (
            "java/JHelper.java",
            "package p; public class JHelper { public static void go() {} }",
        ),
        ("kotlin/KHelper.kt", "package p\nclass KHelper"),
        (
            "kotlin/KUse.kt",
            "package p\nclass KUse { fun make() = JHelper(); fun run() { JHelper.go() } }",
        ),
        (
            "java/JUse.java",
            "package p; public class JUse { public KHelper make() { return new KHelper(); } }",
        ),
        (
            "foreign/Other.kt",
            "package other\nclass Other { fun make() = JHelper() }",
        ),
    ]);
    for (path, owner, name, target_path) in [
        (
            "kotlin/KUse.kt",
            "p::KUse::make",
            "JHelper",
            "java/JHelper.java",
        ),
        (
            "java/JUse.java",
            "p::JUse::make",
            "KHelper",
            "kotlin/KHelper.kt",
        ),
    ] {
        let make = capability_symbol(&facts, path, owner);
        targets(
            CapabilityReferenceQuery::new(&facts, make).named(name, ReferenceKind::Instantiates),
            capability_symbol(&facts, target_path, &format!("p::{name}")),
            "native-jvm-package",
        );
    }
    let run = capability_symbol(&facts, "kotlin/KUse.kt", "p::KUse::run");
    targets(
        CapabilityReferenceQuery::new(&facts, run).named("JHelper.go", ReferenceKind::Calls),
        capability_symbol(&facts, "java/JHelper.java", "p::JHelper::go"),
        "native-qualified-member",
    );
    let other = capability_symbol(&facts, "foreign/Other.kt", "other::Other::make");
    assert!(
        CapabilityReferenceQuery::new(&facts, other)
            .named("JHelper", ReferenceKind::Instantiates)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn jvm_same_package_beats_wildcards_and_explicit_imports_beat_both() {
    let facts = generation(&[
        ("generated/Own.java", "package p; public class Helper {}"),
        ("generated/Other.java", "package q; public class Helper {}"),
        (
            "client/Own.java",
            "package p; import java.util.*; import q.*; public class Own { Helper value; }",
        ),
        (
            "client/Explicit.java",
            "package p; import java.util.*; import q.Helper; public class Explicit { Helper value; }",
        ),
        (
            "client/Wild.kt",
            "package client\nimport java.util.*\nimport q.*\nclass Wild { fun make() = Helper() }",
        ),
    ]);
    for (path, owner, target_path, package, provenance) in [
        (
            "client/Own.java",
            "p::Own::value",
            "generated/Own.java",
            "p",
            "native-jvm-package",
        ),
        (
            "client/Explicit.java",
            "p::Explicit::value",
            "generated/Other.java",
            "q",
            "native-jvm-explicit-import",
        ),
    ] {
        let field = capability_symbol(&facts, path, owner);
        targets(
            CapabilityReferenceQuery::new(&facts, field).named("Helper", ReferenceKind::TypeOf),
            capability_symbol(&facts, target_path, &format!("{package}::Helper")),
            provenance,
        );
    }
    let make = capability_symbol(&facts, "client/Wild.kt", "client::Wild::make");
    targets(
        CapabilityReferenceQuery::new(&facts, make).named("Helper", ReferenceKind::Instantiates),
        capability_symbol(&facts, "generated/Other.java", "q::Helper"),
        "native-jvm-wildcard-import",
    );
}

#[test]
fn jvm_wildcard_ties_and_duplicate_fqns_abstain() {
    let facts = generation(&[
        ("a/Helper.java", "package a; public class Helper {}"),
        ("b/Helper.java", "package b; public class Helper {}"),
        ("one/Duplicate.java", "package d; public class Duplicate {}"),
        ("two/Duplicate.kt", "package d\nclass Duplicate"),
        (
            "client/Use.java",
            "package client; import a.*; import b.*; import d.Duplicate; public class Use { Helper helper; Duplicate duplicate; Missing missing; }",
        ),
    ]);
    for (field, name) in [
        ("helper", "Helper"),
        ("duplicate", "Duplicate"),
        ("missing", "Missing"),
    ] {
        let owner = capability_symbol(&facts, "client/Use.java", &format!("client::Use::{field}"));
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::TypeOf);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn qualified_static_calls_bind_methods_and_missing_or_overloaded_members_abstain() {
    for (path, source, owner, method) in [
        (
            "src/Use.java",
            "class Builder { public static void create() {} public static void overloaded() {} public static void overloaded(int value) {} public void instance() {} private static void hidden() {} } class Use { void run() { Builder.create(); Builder.missing(); Builder.overloaded(); Builder.instance(); Builder.hidden(); } }",
            "Use::run",
            "Builder::create",
        ),
        (
            "src/use.ts",
            "class Builder { static create() {} static overloaded(): void; static overloaded(value: number): void; static overloaded(value?: number) {} instance() {} private static hidden() {} } export function run() { Builder.create(); Builder.missing(); Builder.instance(); Builder.hidden(); }",
            "run",
            "Builder::create",
        ),
        (
            "src/Use.cs",
            "public class Builder { public static void Create() {} public void Instance() {} } public class Use { public void Run() { Builder.Create(); Builder.Missing(); Builder.Instance(); } }",
            "Use::Run",
            "Builder::Create",
        ),
        (
            "src/use.rb",
            "class Builder\n def self.create; end\n def instance; end\nend\ndef run\n Builder.create\n Builder.missing\n Builder.instance\nend\n",
            "run",
            "Builder::create",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let caller = capability_symbol(&facts, path, owner);
        let name = if path.ends_with("cs") {
            "Builder.Create"
        } else {
            "Builder.create"
        };
        targets(
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls),
            capability_symbol(&facts, path, method),
            "native-qualified-member",
        );
        for member in if path.ends_with("cs") {
            ["Missing", "Instance"]
        } else {
            ["missing", "instance"]
        } {
            let reference = CapabilityReferenceQuery::new(&facts, caller)
                .named(&format!("Builder.{member}"), ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        }
        if path.ends_with("java") {
            for member in ["overloaded", "hidden"] {
                assert!(
                    CapabilityReferenceQuery::new(&facts, caller)
                        .named(&format!("Builder.{member}"), ReferenceKind::Calls)
                        .target_symbol_id
                        .is_none()
                );
            }
        }
    }
}

#[test]
fn imported_static_calls_target_the_method_and_respect_value_shadowing() {
    let facts = generation(&[
        (
            "src/builder.ts",
            "export class Builder { static create() {} }",
        ),
        (
            "src/use.ts",
            "import { Builder } from './builder'; export function run() { Builder.create(); Builder.missing(); } export function shadow(Builder: object) { Builder.create(); }",
        ),
    ]);
    let run = capability_symbol(&facts, "src/use.ts", "run");
    targets(
        CapabilityReferenceQuery::new(&facts, run).named("Builder.create", ReferenceKind::Calls),
        capability_symbol(&facts, "src/builder.ts", "Builder::create"),
        "native-import-binding",
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, run)
            .named("Builder.missing", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    let shadow = capability_symbol(&facts, "src/use.ts", "shadow");
    assert!(
        CapabilityReferenceQuery::new(&facts, shadow)
            .named("Builder.create", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn interface_inheritance_preserves_extends_and_class_conformance_implements() {
    for (path, source, base, child, implementation) in [
        (
            "src/Types.swift",
            "protocol Base {}\nprotocol Child: Base {}\nstruct Impl: Base {}\nprotocol Unknown: Missing {}\n",
            "Base",
            "Child",
            "Impl",
        ),
        (
            "src/Types.java",
            "interface Base {} interface Child extends Base {} class Impl implements Base {} interface Unknown extends Missing {}",
            "Base",
            "Child",
            "Impl",
        ),
        (
            "src/types.ts",
            "interface Base {} interface Child extends Base {} class Impl implements Base {} interface Unknown extends Missing {}",
            "Base",
            "Child",
            "Impl",
        ),
        (
            "src/Types.cs",
            "interface Base {} interface Child : Base {} class Impl : Base {} interface Unknown : Missing {}",
            "Base",
            "Child",
            "Impl",
        ),
        (
            "src/Types.kt",
            "interface Base\ninterface Child: Base\nclass Impl: Base\ninterface Unknown: Missing\n",
            "Base",
            "Child",
            "Impl",
        ),
    ] {
        let facts = generation(&[(path, source)]);
        let base = capability_symbol(&facts, path, base);
        let child = capability_symbol(&facts, path, child);
        let implementation = capability_symbol(&facts, path, implementation);
        for (source, kind) in [
            (child, EdgeKind::Extends),
            (implementation, EdgeKind::Implements),
        ] {
            assert!(
                facts
                    .edges()
                    .iter()
                    .any(|edge| edge.source_symbol_id == source.symbol_id
                        && edge.target_symbol_id == base.symbol_id
                        && edge.kind == kind
                        && edge.provenance == "native-exact-same-file"),
                "{path}: {:?}",
                facts.edges()
            );
        }
        let unknown = capability_symbol(&facts, path, "Unknown");
        let missing = facts
            .references()
            .iter()
            .filter(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&unknown.symbol_id)
                    && reference.reference_name == "Missing"
            })
            .collect::<Vec<_>>();
        assert_eq!(missing.len(), 1, "{path}: {missing:?}");
        assert!(missing[0].target_symbol_id.is_none());
    }
}
