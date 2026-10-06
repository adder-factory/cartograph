use super::generic_repair::{assert_base_reference, base_generation};
use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, ReferenceInput, ReferenceKind,
    SymbolInput, assert_confidence, build_capability_generation, capability_symbol,
};

fn assert_call(facts: &CanonicalGenerationFacts, call: &ReferenceInput, target: &SymbolInput) {
    assert_eq!(
        call.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{call:?}"
    );
    assert!(facts.edges().iter().any(|edge| {
        Some(&edge.source_symbol_id) == call.owner_symbol_id.as_ref()
            && edge.target_symbol_id == target.symbol_id
            && edge.kind == EdgeKind::Calls
            && edge.provenance == call.resolution_provenance
            && super::confidence_matches(edge.confidence, call.confidence)
    }));
}

#[test]
fn current_class_calls_cover_managed_cpp_swift_and_ruby_scopes() {
    let fixtures = [
        (
            "src/Worker.java",
            "class Worker { void run() { helper(); this.helper2(); missing(); } void helper() {} void helper2() {} } class Other { void missing() {} }",
            "helper2",
        ),
        (
            "src/Worker.cs",
            "class Worker { void run() { helper(); this.helper2(); missing(); } void helper() {} void helper2() {} } class Other { void missing() {} }",
            "helper2",
        ),
        (
            "src/Worker.kt",
            "class Worker {\n fun run() {\n helper()\n this.helper2()\n missing()\n }\n fun helper() {}\n fun helper2() {}\n}\nclass Other {\n fun missing() {}\n}\n",
            "helper2",
        ),
        (
            "src/worker.cpp",
            "class Worker { public: void run() { helper(); this->helper2(); missing(); } void helper() {} void helper2() {} }; class Other { public: void missing() {} };",
            "helper2",
        ),
        (
            "src/worker.swift",
            "class Worker {\n func run() {\n helper()\n self.helper2()\n missing()\n }\n func helper() {}\n func helper2() {}\n}\nclass Other {\n func missing() {}\n}\n",
            "helper2",
        ),
        (
            "src/worker.rb",
            "class Worker\n def run\n helper()\n self.helper2()\n missing()\n end\n def helper\n end\n def helper2\n end\nend\nclass Other\n def missing\n end\nend\n",
            "helper2",
        ),
    ];
    for (path, source, explicit) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        let owner = capability_symbol(&facts, path, "Worker::run");
        for name in ["helper", explicit] {
            let target = capability_symbol(&facts, path, &format!("Worker::{name}"));
            let call = facts
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                        && reference.reference_kind == ReferenceKind::Calls.as_str()
                        && reference.reference_name.ends_with(name)
                })
                .unwrap_or_else(|| panic!("missing {path}::{name}"));
            assert_eq!(
                call.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{path}::{name}: {call:?}"
            );
            assert_call(&facts, call, target);
            assert_confidence(call.confidence, 1.0);
            let provenance = if call.reference_name.starts_with("this.")
                || call.reference_name.starts_with("self.")
            {
                "native-current-class-call"
            } else {
                "native-exact-lexical"
            };
            assert_eq!(call.resolution_provenance, provenance, "{path}::{name}");
        }
        let missing =
            CapabilityReferenceQuery::new(&facts, owner).named("missing", ReferenceKind::Calls);
        assert!(missing.target_symbol_id.is_none(), "{path}");
    }
}

#[test]
fn current_class_receivers_abstain_for_nested_dynamic_this_and_overloads() {
    let facts = build_capability_generation(
        &[(
            "src/worker.ts",
            "class Worker { run() { function nested() { this.helper(); } this.overloaded(); } helper() {} overloaded(x: number): void; overloaded(x: string): void; }\n",
        )],
        false,
    );
    let nested = capability_symbol(&facts, "src/worker.ts", "Worker::run::nested");
    assert!(
        CapabilityReferenceQuery::new(&facts, nested)
            .named("this.helper", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("this.overloaded", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn lexical_arrow_preserves_this_but_nested_function_does_not() {
    let facts = build_capability_generation(
        &[(
            "src/worker.ts",
            "class Worker { run() { register(() => { this.helper(); }); register(function () { this.helper(); }); } helper() {} }\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    let target = capability_symbol(&facts, "src/worker.ts", "Worker::helper");
    let mut calls = facts
        .references()
        .iter()
        .filter(|call| {
            call.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && call.reference_kind == ReferenceKind::Calls.as_str()
                && call.reference_name == "this.helper"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    calls.sort_unstable_by_key(|call| call.start_byte);
    assert_call(&facts, calls[0], target);
    assert_eq!(calls[0].resolution_provenance, "native-current-class-call");
    assert_confidence(calls[0].confidence, 1.0);
    assert!(calls[1].target_symbol_id.is_none());
}

#[test]
fn receiver_overloads_do_not_prefer_a_different_implemented_signature() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.java",
            "abstract class Worker { abstract void helper(double x); void helper(int x) {} void run() { this.helper(1.0); } }\n",
        )],
        false,
    );
    let overloads = facts
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "Worker::helper")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert!(overloads.iter().any(|symbol| symbol.declaration_only));
    assert!(overloads.iter().any(|symbol| !symbol.declaration_only));
    let owner = capability_symbol(&facts, "src/Worker.java", "Worker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn global_cpp_qualifiers_do_not_suffix_match_a_nested_namespace() {
    let facts = build_capability_generation(
        &[
            ("src/use.cpp", "void use() { ::Foo::run(); }\n"),
            (
                "src/target.cpp",
                "namespace company { class Foo { public: static void run() {} }; }\n",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/use.cpp", "use");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("::Foo::run", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn normalized_java_receivers_do_not_prove_implicit_class_members() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.java",
            "class Worker { void run() { factory().helper(); \"value\".helper(); (factory()).helper(); } void helper() {} }\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/Worker.java", "Worker::run");
    let calls = facts
        .references()
        .iter()
        .filter(|call| {
            call.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && call.reference_kind == ReferenceKind::Calls.as_str()
                && call.reference_name == "helper"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 3);
    for call in calls {
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
}

#[test]
fn normalized_swift_chains_preserve_base_resolution_on_abstention() {
    let facts = build_capability_generation(
        &[(
            "src/worker.swift",
            "class Worker {\n func make() -> Other { Other() }\n func run() { self.make().helper(); make().helper() }\n}\nclass Other {\n func helper() {}\n}\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.swift", "Worker::run");
    let target = capability_symbol(&facts, "src/worker.swift", "Worker::make");
    for name in ["self.make", "make"] {
        let mut calls = facts
            .references()
            .iter()
            .filter(|call| {
                call.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && call.reference_kind == ReferenceKind::Calls.as_str()
                    && call.reference_name == name
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{name}: {calls:?}");
        calls.sort_unstable_by_key(|call| call.end_byte);
        assert_call(&facts, calls[0], target);
        assert_confidence(calls[0].confidence, 1.0);
        let provenance = if name == "make" {
            "native-exact-lexical"
        } else {
            "native-current-class-call"
        };
        assert_eq!(calls[0].resolution_provenance, provenance);
        assert!(calls[1].end_byte > calls[0].end_byte);
        if name == "make" {
            assert_eq!(calls[1].target_symbol_id.as_ref(), Some(&target.symbol_id));
            assert_eq!(calls[1].resolution_provenance, "native-exact-lexical");
        } else {
            assert!(
                calls[1].target_symbol_id.is_none(),
                "{name}: {:?}",
                calls[1]
            );
        }
    }
}

#[test]
fn normalized_kotlin_indexed_receivers_do_not_bind_a_sibling_method() {
    let facts = build_capability_generation(
        &[(
            "src/worker.kt",
            "class Worker {\n val helpers = listOf<() -> Unit>({})\n fun helpers() {}\n fun run(index: Int) { this.helpers[index]() }\n}\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.kt", "Worker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helpers", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert!(facts.symbols().iter().any(|symbol| {
        symbol.qualified_name == "Worker::helpers" && symbol.symbol_kind == "method"
    }));
}

#[test]
fn current_class_receiver_fields_do_not_disappear_from_binding_competition() {
    let facts = build_capability_generation(
        &[(
            "src/worker.ts",
            "class Worker { helper = callback; helper() {} run() { this.helper(); } }\n",
        )],
        false,
    );
    let bindings = facts
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "Worker::helper")
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().any(|symbol| symbol.symbol_kind == "field"));
    assert!(bindings.iter().any(|symbol| symbol.symbol_kind == "method"));
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    for name in ["this.helper", "helper"] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
}

#[test]
fn java_static_overloads_do_not_disappear_from_instance_call_competition() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.java",
            "class Worker { static void helper(double x) {} void helper(int x) {} void run() { this.helper(1.0); } }\n",
        )],
        false,
    );
    let overloads = facts
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "Worker::helper")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert!(
        overloads
            .iter()
            .any(|symbol| symbol.execution.static_member)
    );
    assert!(
        overloads
            .iter()
            .any(|symbol| !symbol.execution.static_member)
    );
    let owner = capability_symbol(&facts, "src/Worker.java", "Worker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn casefold_overloads_do_not_prefer_a_different_implemented_signature() {
    let facts = build_capability_generation(
        &[(
            "src/worker.vb",
            "Public MustInherit Class Worker\n Public MustOverride Sub process(value As Double)\n Public Sub process(value As Integer)\n End Sub\n Public Sub run()\n PROCESS(1.0)\n End Sub\nEnd Class\n",
        )],
        false,
    );
    let overloads = facts
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "Worker::process")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert!(overloads.iter().any(|symbol| symbol.declaration_only));
    assert!(overloads.iter().any(|symbol| !symbol.declaration_only));
    let owner = capability_symbol(&facts, "src/worker.vb", "Worker::run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn receiver_words_in_python_and_ruby_parameters_do_not_prove_class_ownership() {
    let fixtures = [
        (
            "src/worker.py",
            "class Worker:\n    def run(self, this):\n        this.helper()\n    def helper(self):\n        pass\n",
        ),
        (
            "src/worker.rb",
            "class Worker\n def run(this)\n this.helper()\n end\n def helper\n end\nend\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for (path, _) in fixtures {
        let owner = capability_symbol(&facts, path, "Worker::run");
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{path}: {call:?}");
    }
}

#[test]
fn normalized_ruby_receivers_and_swift_locals_preserve_base_resolution() {
    let fixtures = [
        (
            "src/worker.rb",
            "class Worker\n def run\n 42.helper()\n super.helper()\n end\n def helper\n end\nend\n",
        ),
        (
            "src/worker.swift",
            "class Worker {\n func run() {\n let helper = callback\n helper()\n }\n func helper() {}\n}\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let base = base_generation(&fixtures);
    for (path, _) in fixtures {
        let owner = capability_symbol(&facts, path, "Worker::run");
        let calls = facts
            .references()
            .iter()
            .filter(|call| {
                call.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && call.reference_kind == ReferenceKind::Calls.as_str()
                    && call.reference_name == "helper"
            })
            .collect::<Vec<_>>();
        assert!(!calls.is_empty(), "{path}");
        for call in calls {
            assert_base_reference(&base, call);
        }
    }
}

#[test]
fn anonymous_javascript_receivers_abstain_for_full_calls_and_dynamic_companions() {
    let fixtures = [(
        "src/worker.ts",
        "class Worker { run() { register(function () { this.helper(); }); register(function* () { this.helper(); }); register({ run() { this.helper(); } }); } helper() {} }\n",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let calls = facts
        .references()
        .iter()
        .filter(|call| {
            call.reference_kind == ReferenceKind::Calls.as_str()
                && ["this.helper", "helper"].contains(&call.reference_name.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 6);
    for call in calls {
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
}

#[test]
fn kotlin_extension_and_lambda_receivers_abstain() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.kt",
            "class Worker {\n fun Other.run() {\n this.helper()\n helper()\n }\n fun execute() {\n with(other) { this.helper() }\n }\n fun helper() {}\n}\nclass Other {\n fun helper() {}\n}\n",
        )],
        false,
    );
    let calls = facts
        .references()
        .iter()
        .filter(|call| {
            call.reference_kind == ReferenceKind::Calls.as_str()
                && ["this.helper", "helper"].contains(&call.reference_name.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 3);
    for call in calls {
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
}

#[test]
fn unrepresented_callable_parameters_abstain_to_base_lookup() {
    let fixtures = [
        (
            "src/Worker.kt",
            "class Worker {\n fun run(first: () -> Unit, helper: () -> Unit) {\n helper()\n }\n fun helper() {}\n}\nfun helper() {}\n",
        ),
        (
            "src/Worker.cs",
            "class Worker { void run(System.Action helper) { helper(); } void helper() {} }",
        ),
        (
            "src/Worker.vb",
            "Public Class Worker\nPublic Sub Run(first As System.Action, process As System.Action)\nPROCESS()\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let base = base_generation(&fixtures);
    for (path, owner_name, name) in [
        ("src/Worker.kt", "Worker::run", "helper"),
        ("src/Worker.cs", "Worker::run", "helper"),
        ("src/Worker.vb", "Worker::Run", "PROCESS"),
    ] {
        let owner = capability_symbol(&facts, path, owner_name);
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_base_reference(&base, call);
    }
}

#[test]
fn immediate_class_members_precede_globals_and_ambiguity_preserves_base() {
    let facts = build_capability_generation(
        &[(
            "src/worker.cpp",
            "void helper() {} void overloaded() {} class Worker { public: void run() { helper(); overloaded(); } void helper() {} void overloaded(int); void overloaded(double); };\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.cpp", "Worker::run");
    let target = capability_symbol(&facts, "src/worker.cpp", "Worker::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(call.resolution_provenance, "native-exact-lexical");
    assert_confidence(call.confidence, 1.0);
    let ambiguous =
        CapabilityReferenceQuery::new(&facts, owner).named("overloaded", ReferenceKind::Calls);
    let global = capability_symbol(&facts, "src/worker.cpp", "overloaded");
    assert_eq!(ambiguous.target_symbol_id.as_ref(), Some(&global.symbol_id));
    assert_eq!(ambiguous.resolution_provenance, "native-exact-same-file");
}

#[test]
fn suffix_fallback_rejects_same_file_private_members_and_capitalized_object_receivers() {
    let facts = build_capability_generation(
        &[
            (
                "src/use.cpp",
                "namespace company { class Foo { private: static void run() {} }; } void use() { Foo::run(); }\n",
            ),
            (
                "src/Use.cs",
                "class Worker { void Use(IFoo Foo) { Foo.Run(); } } namespace company { public class Foo { public static void Run() {} } }\n",
            ),
        ],
        false,
    );
    for (path, owner_name, name) in [
        ("src/use.cpp", "use", "Foo::run"),
        ("src/Use.cs", "Worker::Use", "Foo.Run"),
    ] {
        let owner = capability_symbol(&facts, path, owner_name);
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{path}: {call:?}");
    }
}

#[test]
fn suffix_fallback_abstains_on_local_aliases_and_template_qualifiers() {
    let facts = build_capability_generation(
        &[
            (
                "src/alias.cpp",
                "namespace actual { class Target { public: static void run() {} }; } using Foo = actual::Target; namespace company { class Foo { public: static void run() {} }; } void use() { Foo::run(); }\n",
            ),
            (
                "src/template.cpp",
                "template<typename Foo> void generic() { Foo::run(); }\n",
            ),
            (
                "src/api.cpp",
                "namespace company { class Foo { public: static void run() {} }; }\n",
            ),
        ],
        false,
    );
    for (path, owner_name) in [("src/alias.cpp", "use"), ("src/template.cpp", "generic")] {
        let owner = capability_symbol(&facts, path, owner_name);
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("Foo::run", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{path}: {call:?}");
    }
}

#[test]
fn normalized_vbnet_factory_receivers_do_not_bind_sibling_methods() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.vb",
            "Public Class Worker\nPublic Sub Run()\nFactory().PROCESS()\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/Worker.vb", "Worker::Run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn explicit_current_class_receivers_keep_static_and_instance_members_distinct() {
    let facts = build_capability_generation(
        &[(
            "src/worker.ts",
            "class Worker { static run() { this.helper(); this.instance(); } static helper() {} instance() {} }\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    let helper = capability_symbol(&facts, "src/worker.ts", "Worker::helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert_call(&facts, call, helper);
    let missing =
        CapabilityReferenceQuery::new(&facts, owner).named("this.instance", ReferenceKind::Calls);
    assert!(missing.target_symbol_id.is_none());
}

#[test]
fn dynamic_companions_preserve_base_for_unproved_current_class_calls() {
    let facts = build_capability_generation(
        &[
            (
                "src/worker.ts",
                "class Worker { run() { this.helper(); this.missing(); } helper() {} }\n",
            ),
            (
                "src/other.ts",
                "export class Other { public helper() {} public missing() {} }\n",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    for name in ["this.missing", "missing"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        if name == "missing" {
            let foreign = capability_symbol(&facts, "src/other.ts", "Other::missing");
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&foreign.symbol_id)
            );
            assert_eq!(reference.resolution_provenance, "native-dynamic-dispatch");
            assert_confidence(reference.confidence, 0.65);
        } else {
            assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        }
    }
    let target = capability_symbol(&facts, "src/worker.ts", "Worker::helper");
    let companion =
        CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_call(&facts, companion, target);
    assert_eq!(companion.resolution_provenance, "native-current-class-call");
    let edge = facts
        .edges()
        .iter()
        .find(|edge| {
            edge.source_symbol_id == owner.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == EdgeKind::Calls
        })
        .unwrap_or_else(|| panic!("missing current-class edge"));
    assert_eq!(
        edge.site_count, 1,
        "a dynamic companion is the same syntactic call"
    );
}

#[test]
fn current_class_receivers_bind_only_the_owning_class() {
    let fixtures = [(
        "src/worker.ts",
        "class Worker { run() { this.helper(); this.missing(); } helper() {} }\nclass Other { helper() {} missing() {} }\n",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/worker.ts", "Worker::run");
    let target = capability_symbol(&facts, "src/worker.ts", "Worker::helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("this.helper", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(call.resolution_provenance, "native-current-class-call");
    assert_confidence(call.confidence, 1.0);
    let missing =
        CapabilityReferenceQuery::new(&facts, owner).named("this.missing", ReferenceKind::Calls);
    assert!(missing.target_symbol_id.is_none());
}

#[test]
fn current_class_resolution_restores_the_frozen_v1_typescript_helper_target() {
    let path = "src/services/user-service.ts";
    let source = include_str!(
        "../../../../cartograph-extract/tests/fixtures/v1_parity/typescript/src/services/user-service.ts"
    );
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../cartograph-extract/tests/fixtures/v1_parity/expected/typescript.json"
    ))
    .unwrap_or_else(|error| panic!("invalid frozen v1 expectation: {error}"));
    assert!(
        expected["edges"]
            .as_array()
            .unwrap_or_else(|| panic!("missing v1 edges"))
            .iter()
            .any(|edge| {
                edge["file"] == path
                    && edge["target_file"] == path
                    && edge["kind"] == "calls"
                    && edge["source"] == "UserService::save"
                    && edge["target"] == "UserService::helper"
            })
    );
    let facts = build_capability_generation(&[(path, source)], false);
    let owner = capability_symbol(&facts, path, "UserService::save");
    let target = capability_symbol(&facts, path, "UserService::helper");
    for name in ["this.helper", "helper"] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_call(&facts, call, target);
        assert_eq!(call.resolution_provenance, "native-current-class-call");
        assert_confidence(call.confidence, 1.0);
    }
}

#[test]
fn recursion_emits_a_calls_edge_and_local_shadowing_abstains() {
    let fixtures = [(
        "src/retry.ts",
        "export function retry(n: number) { if (n > 0) retry(n - 1); }\nexport function shadowed() { const shadowed = () => {}; shadowed(); }\n",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/retry.ts", "retry");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("retry", ReferenceKind::Calls);
    assert_call(&facts, call, owner);
    assert_eq!(call.resolution_provenance, "native-exact-same-file");
    assert_confidence(call.confidence, 1.0);
    let shadowed = capability_symbol(&facts, "src/retry.ts", "shadowed");
    let shadow_call =
        CapabilityReferenceQuery::new(&facts, shadowed).named("shadowed", ReferenceKind::Calls);
    assert_ne!(
        shadow_call.target_symbol_id.as_ref(),
        Some(&shadowed.symbol_id)
    );
}

#[test]
fn objective_c_super_messages_are_not_recursion() {
    let facts = build_capability_generation(
        &[(
            "ios/Foo.mm",
            "@interface Foo : UIView\n@end\n@implementation Foo\n- (instancetype)initWithFrame:(CGRect)frame\n{\n  if (self = [super initWithFrame:frame]) {}\n  return self;\n}\n@end\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "ios/Foo.mm", "Foo::initWithFrame:");
    assert!(
        !facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
            && edge.source_symbol_id == owner.symbol_id
            && edge.target_symbol_id == owner.symbol_id),
        "[super initWithFrame:] targets the superclass, not the method itself"
    );
}

#[test]
fn anonymous_callback_parameters_do_not_create_outer_function_recursion_edges() {
    let facts = build_capability_generation(
        &[(
            "src/retry.ts",
            "export function retry() { consume((retry: () => void) => retry()); consume(function(retry) { retry(); }); }\n",
        )],
        false,
    );
    let calls = facts
        .references()
        .iter()
        .filter(|call| {
            call.reference_kind == ReferenceKind::Calls.as_str() && call.reference_name == "retry"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
    let owner = capability_symbol(&facts, "src/retry.ts", "retry");
    assert!(
        !facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
            && edge.source_symbol_id == owner.symbol_id
            && edge.target_symbol_id == owner.symbol_id)
    );
}

#[test]
fn inline_vbnet_lambda_parameters_do_not_bind_sibling_methods() {
    let facts = build_capability_generation(
        &[(
            "src/Worker.vb",
            "Public Class Worker\nPublic Sub Run()\nUse(Sub(process As System.Action) PROCESS())\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class\n",
        )],
        false,
    );
    let owner = capability_symbol(&facts, "src/Worker.vb", "Worker::Run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn lexical_call_proof_admits_unrelated_parameters_in_long_signatures() {
    let parameters = (0..100)
        .map(|ordinal| format!("argument{ordinal}: number"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("export function retry({parameters}) {{ retry(); }}\n");
    let facts = build_capability_generation(&[("src/retry.ts", &source)], false);
    let owner = capability_symbol(&facts, "src/retry.ts", "retry");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("retry", ReferenceKind::Calls);
    assert_call(&facts, call, owner);
    assert_eq!(call.resolution_provenance, "native-exact-same-file");
    assert_confidence(call.confidence, 1.0);
}

#[test]
fn recursion_respects_parameter_shadowing_and_does_not_emit_reference_self_edges() {
    let facts = build_capability_generation(
        &[(
            "src/retry.ts",
            "export function callback(callback: () => void) { callback(); }\nexport function recurse() { const reference = recurse; recurse(); }\n",
        )],
        false,
    );
    let callback = capability_symbol(&facts, "src/retry.ts", "callback");
    let call =
        CapabilityReferenceQuery::new(&facts, callback).named("callback", ReferenceKind::Calls);
    assert_ne!(call.target_symbol_id.as_ref(), Some(&callback.symbol_id));
    let recurse = capability_symbol(&facts, "src/retry.ts", "recurse");
    let call =
        CapabilityReferenceQuery::new(&facts, recurse).named("recurse", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(call.resolution_provenance, "native-unresolved");
    assert!(
        !facts
            .edges()
            .iter()
            .any(|edge| edge.source_symbol_id == recurse.symbol_id
                && edge.target_symbol_id == recurse.symbol_id
                && edge.kind == EdgeKind::References)
    );
}

#[test]
fn casefolding_abstains_on_case_variants_and_never_applies_to_typescript() {
    let fixtures = [
        (
            "src/worker.vb",
            "Public Class Worker\nPublic Sub Run()\nPROCESS()\nEnd Sub\nPublic Sub Process()\nEnd Sub\nPublic Sub process()\nEnd Sub\nEnd Class\n",
        ),
        ("src/use.ts", "export function use() { PROCESS(); }\n"),
        ("src/api.ts", "export function process() {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_eq!(
        facts.digest(),
        build_capability_generation(&fixtures, true).digest()
    );
    for (path, owner_name) in [("src/worker.vb", "Worker::Run"), ("src/use.ts", "use")] {
        let owner = capability_symbol(&facts, path, owner_name);
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{path}");
    }
}

#[test]
fn casefolding_resolves_public_pascal_routines_and_preserves_base_on_ties() {
    let fixtures = [
        (
            "src/Main.pas",
            "unit UMain;\ninterface\nimplementation\nprocedure Run;\nbegin\n PROCESS();\n DUPLICATE();\nend;\nprocedure Shadow(process: TProc);\nbegin\n PROCESS();\nend;\nend.\n",
        ),
        (
            "src/Api.pas",
            "unit UApi;\ninterface\nprocedure Process;\nprocedure Duplicate;\nimplementation\nprocedure Process;\nbegin\nend;\nprocedure Duplicate;\nbegin\nend;\nend.\n",
        ),
        (
            "src/Other.pas",
            "unit UOther;\ninterface\nprocedure DUPLICATE;\nimplementation\nprocedure DUPLICATE;\nbegin\nend;\nend.\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/Main.pas", "Run");
    let target = capability_symbol(&facts, "src/Api.pas", "Process");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(
        call.resolution_provenance,
        "native-case-insensitive-callable-fallback"
    );
    assert_confidence(call.confidence, 0.5);
    let ambiguous =
        CapabilityReferenceQuery::new(&facts, owner).named("DUPLICATE", ReferenceKind::Calls);
    assert_base_reference(&base_generation(&fixtures), ambiguous);
    let shadow = capability_symbol(&facts, "src/Main.pas", "Shadow");
    let parameter =
        CapabilityReferenceQuery::new(&facts, shadow).named("PROCESS", ReferenceKind::Calls);
    assert!(parameter.target_symbol_id.is_none());
}

#[test]
fn qualified_suffix_never_prefers_an_implementation_to_an_unrelated_declaration() {
    let facts = build_capability_generation(
        &[
            (
                "src/use.cpp",
                "void use() { Foo::run(); Private::hidden(); obj.run(); }\n",
            ),
            (
                "src/impl.cpp",
                "namespace company { class Foo { public: static void run() {} }; class Private { private: static void hidden() {} }; }\n",
            ),
            (
                "src/api.hpp",
                "namespace other { class Foo { public: static void run(); }; }\n",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/use.cpp", "use");
    for name in ["Foo::run", "Private::hidden", "obj.run"] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{name}");
    }
}

#[test]
fn exact_imports_take_precedence_over_proximity_and_missing_imports_abstain() {
    let fixtures = [
        (
            "app/src/use.ts",
            "import { navigate } from '../../library/api'; export function use() { navigate(); }\n",
        ),
        (
            "app/src/missing.ts",
            "import { navigate } from './missing-api'; export function missing() { navigate(); }\n",
        ),
        ("app/src/server.ts", "export function navigate() {}\n"),
        ("library/api.ts", "export function navigate() {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "app/src/use.ts", "use");
    let target = capability_symbol(&facts, "library/api.ts", "navigate");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("navigate", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(call.resolution_provenance, "native-import-binding");
    let missing = capability_symbol(&facts, "app/src/missing.ts", "missing");
    let call =
        CapabilityReferenceQuery::new(&facts, missing).named("navigate", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none());
    assert_eq!(call.resolution_provenance, "native-unresolved-import");
}

#[test]
fn python_self_and_bare_javascript_methods_preserve_unproved_receiver_boundaries() {
    let fixtures = [
        (
            "src/worker.py",
            "class Worker:\n    def run(self):\n        self.helper()\n    def helper(self):\n        pass\n\ndef retry():\n    retry = lambda: None\n    retry()\n",
        ),
        (
            "src/worker.ts",
            "class Worker { retry() { retry(); } helper() {} }\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let python = capability_symbol(&facts, "src/worker.py", "Worker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, python).named("self.helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none());
    let python_retry = capability_symbol(&facts, "src/worker.py", "retry");
    let call =
        CapabilityReferenceQuery::new(&facts, python_retry).named("retry", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none());
    let retry = capability_symbol(&facts, "src/worker.ts", "Worker::retry");
    let call = CapabilityReferenceQuery::new(&facts, retry).named("retry", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generic_resolution_facts_are_identical_for_all_supported_worker_counts() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_else(|error| panic!("cannot create resolution fixture: {error}"));
    let fixtures = [
        (
            "src/worker.ts",
            "class Worker { run() { this.helper(); this.missing(); } helper() {} }\nexport function retry() { retry(); }\n",
        ),
        ("src/use.cpp", "void use() { Foo::run(); }\n"),
        (
            "src/api.cpp",
            "namespace company { class Foo { public: static void run() {} }; }\n",
        ),
        ("alpha/use.ts", "export function use() { navigate(); }\n"),
        ("alpha/api.ts", "export function navigate() {}\n"),
        ("beta/api.ts", "export function navigate() {}\n"),
        (
            "src/worker.vb",
            "Public Class Worker\nPublic Sub Run()\nPROCESS()\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class\n",
        ),
    ];
    for (path, source) in fixtures {
        let path = directory.path().join(path);
        std::fs::create_dir_all(
            path.parent()
                .unwrap_or_else(|| panic!("missing fixture parent")),
        )
        .unwrap_or_else(|error| panic!("cannot create fixture directory: {error}"));
        std::fs::write(path, source)
            .unwrap_or_else(|error| panic!("cannot write fixture: {error}"));
    }
    let serial = super::build(directory.path(), 1).await;
    for workers in [2, 4, 8, 16] {
        let parallel = super::build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers} workers"
        );
    }
}

#[test]
fn proximity_is_a_lower_confidence_unique_best_directory_fallback() {
    let fixtures = [
        (
            "apps/alpha/src/use.ts",
            "export function use() { navigate(); tied(); }\n",
        ),
        (
            "apps/alpha/src/server.ts",
            "export function navigate() {} export function tied() {}\n",
        ),
        ("apps/beta/src/server.ts", "export function navigate() {}\n"),
        ("apps/alpha/src/other.ts", "export function tied() {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let reversed = build_capability_generation(&fixtures, true);
    assert_eq!(facts.digest(), reversed.digest());
    let owner = capability_symbol(&facts, "apps/alpha/src/use.ts", "use");
    let target = capability_symbol(&facts, "apps/alpha/src/server.ts", "navigate");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("navigate", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(call.resolution_provenance, "native-path-proximity-fallback");
    assert_confidence(call.confidence, 0.7);
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("tied", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn proximity_abstains_across_anonymous_callback_bindings() {
    let facts = build_capability_generation(
        &[
            (
                "apps/alpha/src/use.ts",
                "export function use() { consume((navigate: () => void) => navigate()); }\n",
            ),
            ("apps/alpha/src/api.ts", "export function navigate() {}\n"),
            ("apps/beta/src/api.ts", "export function navigate() {}\n"),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "apps/alpha/src/use.ts", "use");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("navigate", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn qualified_suffix_requires_a_segment_boundary_and_unique_target() {
    let fixtures = [
        (
            "src/use.cpp",
            "void use() { Foo::run(); Bar::wrong(); Dup::call(); }\n",
        ),
        (
            "src/api.cpp",
            "namespace company { class Foo { public: static void run() {} }; class FooBar { public: static void wrong() {} }; class Dup { public: static void call() {} }; }\nnamespace other { class Dup { public: static void call() {} }; }\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/use.cpp", "use");
    let target = capability_symbol(&facts, "src/api.cpp", "company::Foo::run");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("Foo::run", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(
        call.resolution_provenance,
        "native-qualified-suffix-fallback"
    );
    assert_confidence(call.confidence, 0.85);
    for name in ["Bar::wrong", "Dup::call"] {
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named(name, ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn case_insensitive_callable_lookup_stays_language_and_scope_bound() {
    let fixtures = [
        (
            "src/worker.vb",
            "Public Class Worker\nPublic Sub Run()\nPROCESS()\nMISSING()\nEnd Sub\nPublic Sub Process()\nEnd Sub\nEnd Class\nPublic Class Other\nPublic Sub PROCESS()\nEnd Sub\nPublic Sub Missing()\nEnd Sub\nEnd Class\n",
        ),
        ("src/foreign.ts", "export function PROCESS() {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "src/worker.vb", "Worker::Run");
    let target = capability_symbol(&facts, "src/worker.vb", "Worker::Process");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("PROCESS", ReferenceKind::Calls);
    assert_call(&facts, call, target);
    assert_eq!(
        call.resolution_provenance,
        "native-case-insensitive-callable-fallback"
    );
    assert_confidence(call.confidence, 0.5);
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("MISSING", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}
