//! Inheritance preserves lexical bindings; imported return types are prepared in both paths.

use super::*;

const INHERITED_PROVENANCE: &str = "native-inherited-receiver-type";
const RETURN_CONFIDENCE: f32 = 0.9;

const RETURN_FIXTURES: &[(&str, &str)] = &[
    (
        "model/Product.java",
        "package model; public class Product { public void commit() {} }",
    ),
    (
        "consumer/Builder.java",
        "package consumer; import model.*; public class Builder { public Product build() { return null; } public Object opaque() { return null; } public void run(Builder b) { b.build().commit(); } public void unresolved(Builder b) { b.opaque().commit(); } }",
    ),
    (
        "src/model/product.ts",
        "export class Product { commit() {} } export class Decoy { commit() {} }",
    ),
    ("src/model/index.ts", "export * from './product';"),
    (
        "src/consumer/builder.ts",
        "import { Product } from '../model'; export class Builder { build(): Product { return new Product(); } opaque() { return new Product(); } run(b: Builder) { b.build().commit(); } unresolved(b: Builder) { b.opaque().commit(); } }",
    ),
];

const JAVA_RETURN: (&str, &str, &str, &str) = (
    "consumer/Builder.java",
    "consumer::Builder",
    "model/Product.java",
    "model::Product::commit",
);
const TYPESCRIPT_RETURN: (&str, &str, &str, &str) = (
    "src/consumer/builder.ts",
    "Builder",
    "src/model/product.ts",
    "Product::commit",
);
const RETURN_CALLS: &[(&str, &str, &str, &str)] = &[JAVA_RETURN, TYPESCRIPT_RETURN];

#[test]
fn inherited_receiverless_calls_preserve_exact_lexical_functions() {
    let fixtures = [(
        "test.scala",
        "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(): Unit = { def ping(): Unit = (); ping() }\n}\n",
    )];
    let facts = build_capability_generation(&fixtures, false);
    types_track::assert_target(
        &facts,
        ("test.scala", "Child::run", "ping"),
        ("test.scala", "Child::run::ping", EXACT_LEXICAL_PROVENANCE),
    );
    let call = call(&facts, ("test.scala", "Child::run", "ping"));
    assert_confidence(call.confidence, EXACT_LEXICAL_CONFIDENCE);
    generic_repair::assert_base_reference(&generic_repair::base_generation(&fixtures), call);
}

#[test]
fn inherited_receiverless_calls_abstain_for_parameters_and_local_bindings() {
    for source in [
        "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(ping: () => Unit): Unit = { ping() }\n}\n",
        "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(): Unit = { val ping: () => Unit = () => (); ping() }\n}\n",
    ] {
        let fixtures = [("test.scala", source)];
        let facts = build_capability_generation(&fixtures, false);
        let reference = call(&facts, ("test.scala", "Child::run", "ping"));
        assert_ne!(reference.resolution_provenance, INHERITED_PROVENANCE);
        generic_repair::assert_base_reference(
            &generic_repair::base_generation(&fixtures),
            reference,
        );
    }
}

#[test]
fn ambiguous_local_overloads_abstain_before_inherited_lookup() {
    let fixtures = [(
        "test.scala",
        "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(): Unit = { def ping(value: Int): Unit = (); def ping(value: String): Unit = (); ping(1) }\n}\n",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let reference = call(&facts, ("test.scala", "Child::run", "ping"));
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    generic_repair::assert_base_reference(&generic_repair::base_generation(&fixtures), reference);
}

#[test]
fn explicit_inherited_calls_ignore_receiverless_local_shadowing() {
    let facts = build_capability_generation(
        &[(
            "test.scala",
            "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(): Unit = { def ping(): Unit = (); this.ping() }\n def plain(): Unit = { ping() }\n}\n",
        )],
        false,
    );
    for (caller, name) in [("Child::run", "this.ping"), ("Child::plain", "ping")] {
        types_track::assert_target(
            &facts,
            ("test.scala", caller, name),
            ("test.scala", "Base::ping", INHERITED_PROVENANCE),
        );
    }
}

#[test]
fn normalized_dart_this_calls_keep_explicit_instance_inheritance() {
    let facts = build_capability_generation(
        &[(
            "test.dart",
            "class Base { void ping() {} } class Child extends Base { void run() { void ping() {} this.ping(); } }",
        )],
        false,
    );
    types_track::assert_target(
        &facts,
        ("test.dart", "Child::run", "ping"),
        ("test.dart", "Base::ping", INHERITED_PROVENANCE),
    );
}

#[test]
fn normalized_dart_this_calls_keep_direct_members_despite_local_shadowing() {
    let facts = build_capability_generation(
        &[(
            "test.dart",
            "class Child { void ping() {} void run() { void ping() {} void missing() {} this.ping(); this.missing(); } void local() { void ping() {} ping(); } }",
        )],
        false,
    );
    types_track::assert_target(
        &facts,
        ("test.dart", "Child::run", "ping"),
        ("test.dart", "Child::ping", "native-current-class-call"),
    );
    let missing = call(&facts, ("test.dart", "Child::run", "missing"));
    assert!(missing.target_symbol_id.is_none(), "{missing:?}");
    types_track::assert_target(
        &facts,
        ("test.dart", "Child::local", "ping"),
        ("test.dart", "Child::local::ping", EXACT_LEXICAL_PROVENANCE),
    );
}

#[test]
fn normalized_dart_this_calls_ignore_free_functions_in_lexical_and_import_tiers() {
    for source in [
        "void ping() {} class Base { void ping() {} } class Child extends Base { void run() { this.ping(); } }",
        "import 'free.dart'; class Base { void ping() {} } class Child extends Base { void run() { this.ping(); } }",
        "class Base { void ping() {} } class Child extends Base { void run() { this.ping(); } }",
    ] {
        let facts = build_capability_generation(
            &[("test.dart", source), ("free.dart", "void ping() {}")],
            false,
        );
        types_track::assert_target(
            &facts,
            ("test.dart", "Child::run", "ping"),
            ("test.dart", "Base::ping", INHERITED_PROVENANCE),
        );
    }
}

#[test]
fn normalized_dart_this_calls_with_missing_members_abstain_from_free_functions() {
    let facts = build_capability_generation(
        &[
            (
                "test.dart",
                "import 'free.dart'; void local() {} class Base {} class Child extends Base { void run() { this.local(); this.remote(); } }",
            ),
            ("free.dart", "void remote() {}"),
        ],
        false,
    );
    for name in ["local", "remote"] {
        let reference = call(&facts, ("test.dart", "Child::run", name));
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn lower_confidence_inheritance_preserves_an_exact_base_target() {
    let facts = build_capability_generation(
        &[(
            "test.scala",
            "class Base { def ping(): Unit = () }\nclass Child extends Base {\n def run(): Unit = { def ping(): Unit = (); ping() }\n}\n",
        )],
        false,
    );
    let local = capability_symbol(&facts, "test.scala", "Child::run::ping");
    let inherited = capability_symbol(&facts, "test.scala", "Base::ping");
    let base = ReferenceResolution::resolved(ResolvedTarget {
        symbol_id: local.symbol_id.clone(),
        kind: SymbolKind::Function,
        confidence: EXACT_LEXICAL_CONFIDENCE,
        provenance: EXACT_LEXICAL_PROVENANCE,
    });
    let receiver = ResolvedTarget {
        symbol_id: inherited.symbol_id.clone(),
        kind: SymbolKind::Method,
        confidence: RETURN_CONFIDENCE,
        provenance: INHERITED_PROVENANCE,
    };
    let result = receiver_resolution::prefer_base(base, Some(receiver));
    let target = result
        .target
        .unwrap_or_else(|| panic!("exact base remains resolved"));
    assert_eq!(target.symbol_id, local.symbol_id);
    assert_eq!(target.provenance, EXACT_LEXICAL_PROVENANCE);
    assert_confidence(target.confidence, EXACT_LEXICAL_CONFIDENCE);
}

#[test]
fn java_declared_return_chains_resolve_after_wildcard_imports() {
    let facts = build_capability_generation(RETURN_FIXTURES, false);
    assert_return_chain(&facts, JAVA_RETURN);
    let reversed = build_capability_generation(RETURN_FIXTURES, true);
    assert_return_chain(&reversed, JAVA_RETURN);
    assert_eq!(facts.digest(), reversed.digest());
}

#[test]
fn typescript_declared_return_chains_resolve_after_barrel_exports() {
    let facts = build_capability_generation(RETURN_FIXTURES, false);
    assert_return_chain(&facts, TYPESCRIPT_RETURN);
    let reversed = build_capability_generation(RETURN_FIXTURES, true);
    assert_return_chain(&reversed, TYPESCRIPT_RETURN);
    assert_eq!(facts.digest(), reversed.digest());
}

fn call<'a>(
    facts: &'a CanonicalGenerationFacts,
    (path, owner, name): (&str, &str, &str),
) -> &'a ReferenceInput {
    CapabilityReferenceQuery::new(facts, capability_symbol(facts, path, owner))
        .named(name, ReferenceKind::Calls)
}

fn assert_return_chains(facts: &CanonicalGenerationFacts) {
    for &case in RETURN_CALLS {
        assert_return_chain(facts, case);
    }
}

fn assert_return_chain(
    facts: &CanonicalGenerationFacts,
    (path, owner, target_path, target): (&str, &str, &str, &str),
) {
    let caller = format!("{owner}::run");
    types_track::assert_target(
        facts,
        (path, &caller, "commit"),
        (target_path, target, "native-declared-return-receiver"),
    );
    assert_confidence(
        call(facts, (path, &caller, "commit")).confidence,
        RETURN_CONFIDENCE,
    );
    let caller = format!("{owner}::unresolved");
    types_track::assert_abstains(facts, (path, &caller, "commit"));
}

fn assert_native_calls(memory: &CanonicalGenerationFacts, spilled: &CanonicalGenerationFacts) {
    for &(path, owner, _, _) in RETURN_CALLS {
        for method in ["run", "unresolved"] {
            let caller = format!("{owner}::{method}");
            let expected = call(memory, (path, &caller, "commit"));
            let actual = spilled
                .references()
                .iter()
                .find(|actual| {
                    actual.file_id == expected.file_id
                        && actual.owner_symbol_id == expected.owner_symbol_id
                        && actual.start_byte == expected.start_byte
                        && actual.end_byte == expected.end_byte
                })
                .unwrap_or_else(|| panic!("spilled terminal call exists"));
            assert_eq!(actual, expected);
            if let Some(target) = &actual.target_symbol_id {
                assert!(
                    spilled
                        .edges()
                        .iter()
                        .any(|edge| edge.kind == EdgeKind::Calls
                            && Some(&edge.source_symbol_id) == actual.owner_symbol_id.as_ref()
                            && &edge.target_symbol_id == target
                            && edge.provenance == actual.resolution_provenance)
                );
            }
        }
    }
}

fn write_returns(root: &std::path::Path) {
    for (path, source) in RETURN_FIXTURES {
        let target = root.join(path);
        fs::create_dir_all(target.parent().unwrap_or(root))
            .unwrap_or_else(|error| panic!("return parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("return fixture: {error}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_return_chains_survive_native_pipeline_parallelism() {
    let directory = tempdir().unwrap_or_else(|error| panic!("return directory: {error}"));
    write_returns(directory.path());
    let expected = build_capability_generation(RETURN_FIXTURES, false);
    for workers in [1, 2, 4, 8, 16] {
        let actual = build_with_config(directory.path(), workers, config(workers)).await;
        assert_return_chains(actual.facts());
        assert_native_calls(&expected, actual.facts());
    }
}
