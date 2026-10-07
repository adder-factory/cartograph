//! Explicit receiver regressions through native extraction and canonical resolution.
use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, ReferenceKind,
    build_capability_generation, capability_symbol,
};

struct Case {
    path: &'static str,
    source: &'static str,
    caller: &'static str,
    reference: &'static str,
    target: &'static str,
}

const RECEIVER_WORKER_COUNTS: &[usize] = &[2, 4, 8, 16];

const CASES: &[Case] = &[
    Case {
        path: "test.java",
        source: "class Repo { void save() {} } class Other { void save() {} } class Service { void run(Repo param) { param.save(); } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.cs",
        source: "using System; class Repo { public void Save() {} } class Other { public void Save() {} } class Service { void Run(Repo param) { param.Save(); } }",
        caller: "Service::Run",
        reference: "param.Save",
        target: "Repo::Save",
    },
    Case {
        path: "test.kt",
        source: "class Repo {\n fun save() {}\n}\nclass Other {\n fun save() {}\n}\nclass Service(val field: Repo) {\n fun run(param: Repo) {\n param.save()\n }\n}\n",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.swift",
        source: "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { func run(param: Repo) { param.save() } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.dart",
        source: "class Repo { void save() {} } class Other { void save() {} } class Service { void run(Repo param) { param.save(); } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.cpp",
        source: "class Repo { public: void save() {} }; class Other { public: void save() {} }; void run(Repo *param) { param->save(); }",
        caller: "run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.scala",
        source: "class Repo { def save(): Unit = {} }\nclass Other { def save(): Unit = {} }\nclass Service(field: Repo) { def run(param: Repo): Unit = { param.save() } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.rb",
        source: "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n local = Repo.new\n local.save\nend",
        caller: "run",
        reference: "local.save",
        target: "Repo::save",
    },
    Case {
        path: "test.cls",
        source: "public class Repo { public void save() {} } public class Other { public void save() {} } public class Service { void run(Repo param) { param.save(); } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.sol",
        source: "contract Repo { function save() public {} } contract Other { function save() public {} } contract Service { function run(Repo param) public { param.save(); } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.ml",
        source: "class repo = object method save = () end\nclass other = object method save = () end\nlet run () = let local = new repo in local#save",
        caller: "run",
        reference: "save",
        target: "repo.save",
    },
    Case {
        path: "test.ps1",
        source: "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $local = [Repo]::new(); $local.Save() }",
        caller: "Run",
        reference: "Save",
        target: "Repo::Save",
    },
    Case {
        path: "test.pas",
        source: "unit Test; interface type TRepo = class procedure Save(); end; TOther = class procedure Save(); end; implementation procedure Run(param: TRepo); begin param.Save(); end; end.",
        caller: "Run",
        reference: "param.Save",
        target: "TRepo::Save",
    },
    Case {
        path: "test.m",
        source: "@interface Repo\n- (void)save;\n@end\n@interface Other\n- (void)save;\n@end\nvoid run(Repo *param) { [param save]; }",
        caller: "run",
        reference: "param.save",
        target: "Repo::save",
    },
    Case {
        path: "test.ts",
        source: "class Repo { save() {} } class Other { save() {} } class Service { run(param: Repo) { param.save(); } }",
        caller: "Service::run",
        reference: "param.save",
        target: "Repo::save",
    },
];

#[test]
fn types_track_explicit_parameters_and_constructors_have_exact_targets() {
    for case in CASES {
        let fixtures = [(case.path, case.source)];
        let facts = build_capability_generation(&fixtures, false);
        assert_eq!(
            facts.digest(),
            build_capability_generation(&fixtures, true).digest()
        );
        assert_target(
            &facts,
            (case.path, case.caller, case.reference),
            (case.path, case.target, "native-explicit-receiver-type"),
        );
    }
}

#[test]
fn types_track_never_substitutes_a_namesake_method_for_a_missing_member() {
    for case in CASES {
        let name = case
            .target
            .rsplit("::")
            .next()
            .unwrap_or(case.target)
            .rsplit('.')
            .next()
            .unwrap_or(case.target);
        let source = case.source.replacen(name, "unrelated", 1);
        let facts = build_capability_generation(&[(case.path, &source)], false);
        assert_abstains(&facts, (case.path, case.caller, case.reference));
    }
}

pub(super) fn assert_abstains(facts: &CanonicalGenerationFacts, source: (&str, &str, &str)) {
    let caller = capability_symbol(facts, source.0, source.1);
    let reference =
        CapabilityReferenceQuery::new(facts, caller).named(source.2, ReferenceKind::Calls);
    assert_ne!(
        reference.resolution_provenance, "native-explicit-receiver-type",
        "{} {}",
        source.0, source.2
    );
    assert_ne!(
        reference.resolution_provenance, "native-declared-return-receiver",
        "{} {}",
        source.0, source.2
    );
}

#[test]
fn types_track_declared_return_chains_resolve_only_the_declared_class() {
    let fixtures = [
        (
            "service.java",
            "class Product { void commit() {} } class Decoy { void commit() {} } class Builder { Product build() { return null; } void call(Builder value) { value.build().commit(); } }",
        ),
        (
            "service.cs",
            "class Product { public void Commit() {} } class Decoy { public void Commit() {} } class Builder { Product Build() { return null; } void Call(Builder value) { value.Build().Commit(); } }",
        ),
        (
            "service.ts",
            "class Product { commit() {} } class Decoy { commit() {} } class Builder { build(): Product { return new Product(); } call(value: Builder) { value.build().commit(); } }",
        ),
        (
            "service.cpp",
            "class Product { public: void commit() {} }; class Decoy { public: void commit() {} }; class Builder { Product build() { return Product(); } void call(Builder value) { value.build().commit(); } };",
        ),
    ];
    for (path, source) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        let (caller, reference, target) = if path == "service.cs" {
            ("Builder::Call", "Commit", "Product::Commit")
        } else if path == "service.cpp" {
            ("Builder::call", "value.build().commit", "Product::commit")
        } else {
            ("Builder::call", "commit", "Product::commit")
        };
        assert_target(
            &facts,
            (path, caller, reference),
            (path, target, "native-declared-return-receiver"),
        );
    }
}

#[test]
fn types_track_fields_and_constructor_locals_retain_the_declared_receiver() {
    let fixtures = [
        (
            "test.java",
            "class Repo { void save() {} } class Other { void save() {} } class Service { Repo field; void run() { Repo local = new Repo(); local.save(); field.save(); } }",
            "Service::run",
        ),
        (
            "test.cs",
            "class Repo { public void save() {} } class Other { public void save() {} } class Service { Repo field; void run() { var local = new Repo(); local.save(); field.save(); } }",
            "Service::run",
        ),
        (
            "test.kt",
            "class Repo {\n fun save() {}\n}\nclass Other {\n fun save() {}\n}\nclass Service(val field: Repo) {\n fun run() {\n val local = Repo()\n local.save()\n field.save()\n }\n}\n",
            "Service::run",
        ),
        (
            "test.swift",
            "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { var field: Repo; func run() { let local = Repo(); local.save(); field.save() } }",
            "Service::run",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } class Service { Repo field; void run() { final local = Repo(); local.save(); field.save(); } }",
            "Service::run",
        ),
        (
            "test.scala",
            "class Repo { def save(): Unit = {} }\nclass Other { def save(): Unit = {} }\nclass Service(field: Repo) { def run(): Unit = { val local = new Repo(); local.save(); field.save() } }",
            "Service::run",
        ),
        (
            "test.cls",
            "public class Repo { public void save() {} } public class Other { public void save() {} } public class Service { Repo field; void run() { Repo local = new Repo(); local.save(); field.save(); } }",
            "Service::run",
        ),
        (
            "test.sol",
            "contract Repo { function save() public {} } contract Other { function save() public {} } contract Service { Repo field; function run() public { Repo local = new Repo(); local.save(); field.save(); } }",
            "Service::run",
        ),
    ];
    for (path, source, caller) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        for reference in ["local.save", "field.save"] {
            assert_target(
                &facts,
                (path, caller, reference),
                (path, "Repo::save", "native-explicit-receiver-type"),
            );
        }
    }
    let source = "class Repo { save() {} } class Other { save() {} } class Service { field: Repo; run(field: Other) { const local = new Repo(); local.save(); this.field.save(); } }";
    let facts = build_capability_generation(&[("test.ts", source)], false);
    for reference in ["local.save", "this.field.save"] {
        assert_target(
            &facts,
            ("test.ts", "Service::run", reference),
            ("test.ts", "Repo::save", "native-explicit-receiver-type"),
        );
    }
}

#[test]
fn types_track_swift_chains_keep_the_outer_call_site() {
    let source = "class Product { func commit() {} }\nclass Decoy { func commit() {} }\nclass Builder { func build() -> Product { return Product() }\nfunc call(value: Builder) { value.build().commit() } }";
    let facts = build_capability_generation(&[("service.swift", source)], false);
    let caller = capability_symbol(&facts, "service.swift", "Builder::call");
    let target = capability_symbol(&facts, "service.swift", "Product::commit");
    let call = "value.build().commit";
    let start = source.find(call).unwrap_or_else(|| panic!("chain site"));
    let end = start + call.len();
    let references: Vec<_> = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&caller.symbol_id)
                && reference.reference_kind == "calls"
                && reference.start_byte
                    == u64::try_from(start).unwrap_or_else(|error| panic!("{error}"))
                && reference.end_byte
                    == u64::try_from(end).unwrap_or_else(|error| panic!("{error}"))
        })
        .collect();
    assert_eq!(references.len(), 1);
    assert_eq!(
        references[0].target_symbol_id.as_ref(),
        Some(&target.symbol_id)
    );
    assert_eq!(
        references[0].resolution_provenance,
        "native-declared-return-receiver"
    );
    assert!(facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
        && edge.source_symbol_id == caller.symbol_id
        && edge.target_symbol_id == target.symbol_id
        && edge.provenance == "native-declared-return-receiver"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn types_track_receiver_and_declared_return_edges_are_worker_invariant() {
    let directory = super::tempdir().unwrap_or_else(|error| panic!("receiver fixture: {error}"));
    let fixtures = [
        (
            "models.ts",
            "export class Product { commit() {} } export class Builder { build(): Product { return new Product(); } }",
        ),
        (
            "service.ts",
            "import { Builder } from './models'; export function call(value: Builder) { value.build().commit(); }",
        ),
    ];
    for (path, source) in fixtures
        .into_iter()
        .chain(CASES.iter().map(|case| (case.path, case.source)))
    {
        super::fs::write(directory.path().join(path), source)
            .unwrap_or_else(|error| panic!("fixture write: {error}"));
    }
    let serial = super::build(directory.path(), 1).await;
    for case in CASES {
        assert_target(
            serial.facts(),
            (case.path, case.caller, case.reference),
            (case.path, case.target, "native-explicit-receiver-type"),
        );
    }
    assert_target(
        serial.facts(),
        ("service.ts", "call", "commit"),
        (
            "models.ts",
            "Product::commit",
            "native-declared-return-receiver",
        ),
    );
    for &workers in RECEIVER_WORKER_COUNTS {
        let parallel = super::build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers} workers"
        );
    }
}

#[test]
fn types_track_shadowing_reassignment_and_generic_receivers_abstain() {
    let fixtures = [
        (
            "test.java",
            "class Repo { void save() {} } class Other { void save() {} } class Service { void run(Repo param) { consume(param -> param.save()); } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } void run() { Other Repo() { return Other(); } final local = Repo(); local.save(); }",
            "run",
            "local.save",
        ),
        (
            "test.java",
            "class Repo { void save() {} } class Other { void save() {} } class Service<Repo> { void run(Repo param) { param.save(); } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.java",
            "class Repo { void save() {} } class Other { void save() {} } class Service { void run(Repo[] param) { param.save(); } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.ts",
            "class Repo { save() {} } class Other { save() {} } class Service { run<Repo>(param: Repo) { param.save(); } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.rb",
            "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n local = Repo.new\n local = unknown\n local.save\nend",
            "run",
            "local.save",
        ),
        (
            "test.ps1",
            "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $local = [Repo]::new(); $LOCAL = Get-Unknown; $local.Save() }",
            "Run",
            "Save",
        ),
        (
            "test.ps1",
            "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $param = [Repo]::new(); foreach ($param in $unknown) { $param.Save() } }",
            "Run",
            "Save",
        ),
        (
            "test.ts",
            "class Repo { save() {} } class Other { save() {} } class Service { run(param: Repo) { use(({param}) => param.save()); } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.swift",
            "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { func run(param: [Repo]) { param.save() } }",
            "Service::run",
            "param.save",
        ),
    ];
    for (path, source, caller, reference) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_abstains(&facts, (path, caller, reference));
    }
}

#[test]
fn types_track_nested_and_loop_bindings_never_restore_an_outer_receiver() {
    let fixtures = [
        (
            "test.cpp",
            "class Repo { public: void save() {} }; class Other { public: void save() {} }; template<class Repo> void run(Repo param) { param.save(); }",
            "run",
            "param.save",
        ),
        (
            "test.cpp",
            "class Repo { public: void save() {} }; class Other { public: void save() {} }; void run(Repo param) { auto f = [param = Other()]() { param.save(); }; }",
            "run",
            "param.save",
        ),
        (
            "test.swift",
            "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { func run(param: Repo, others: [Other]) { for param in others { param.save() } } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.rb",
            "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n local = Repo.new\n for local in unknown\n puts local\n end\n local.save\nend",
            "run",
            "local.save",
        ),
        (
            "test.ps1",
            "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $param = [Repo]::new(); foreach ($param in $unknown) { Write-Output $param }; $param.Save() }",
            "Run",
            "Save",
        ),
        (
            "test.rb",
            "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n local = Repo.new\n unknown.each do\n local = unknown\n end\n local.save\nend",
            "run",
            "local.save",
        ),
        (
            "test.cs",
            "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { void Run() { var param = new Repo(); if (flag) { param = Unknown(); } param.Save(); } }",
            "Service::Run",
            "param.Save",
        ),
    ];
    for (path, source, caller, reference) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_abstains(&facts, (path, caller, reference));
    }
}

#[test]
fn types_track_return_chains_reject_containers_primitives_and_missing_annotations() {
    for return_type in ["int", "Product[]", "List<Product>"] {
        let source = format!(
            "class Product {{ void commit() {{}} }} class Other {{ void commit() {{}} }} class Builder {{ {return_type} build(Product argument) {{ return null; }} void call(Builder value) {{ value.build(null).commit(); }} }}"
        );
        let facts = build_capability_generation(&[("test.java", &source)], false);
        assert_abstains(&facts, ("test.java", "Builder::call", "commit"));
    }
    let source = "class Product { commit() {} } class Other { commit() {} } class Builder { build(argument: Product) { return argument; } call(value: Builder) { value.build(new Product()).commit(); } }";
    let facts = build_capability_generation(&[("test.ts", source)], false);
    assert_abstains(&facts, ("test.ts", "Builder::call", "commit"));
}

#[test]
fn types_track_declared_returns_do_not_bind_a_shadowed_type_parameter() {
    let fixtures = [
        (
            "test.cpp",
            "class Product { public: void commit() {} }; class Other { public: void commit() {} }; template<class Product> class Builder { public: Product build() {} }; void run(Builder<Other> value) { value.build().commit(); }",
            "run",
            "value.build().commit",
        ),
        (
            "test.java",
            "class Product { void commit() {} } class Other { void commit() {} } class Builder<Product> { Product build() { return null; } void call(Builder value) { value.build().commit(); } }",
            "Builder::call",
            "commit",
        ),
        (
            "test.ts",
            "class Product { commit() {} } class Other { commit() {} } class Builder { build<Product>(): Product { return unknown; } call(value: Builder) { value.build().commit(); } }",
            "Builder::call",
            "commit",
        ),
    ];
    for (path, source, caller, reference) in fixtures {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_abstains(&facts, (path, caller, reference));
    }
}

#[test]
fn types_track_cpp_default_and_variadic_parameters_cannot_name_project_classes() {
    for parameter in ["class Repo = Other", "class... Repo"] {
        let source = format!(
            "class Repo {{ public: void save() {{}} }}; class Other {{ public: void save() {{}} }}; template<{parameter}> void run(Repo param) {{ param.save(); }}"
        );
        let facts = build_capability_generation(&[("test.cpp", &source)], false);
        assert_abstains(&facts, ("test.cpp", "run", "param.save"));
    }
}

#[test]
fn types_track_class_factory_chains_require_a_static_factory() {
    for modifier in ["", "static "] {
        let source = format!(
            "class Product {{ commit() {{}} }} class Other {{ commit() {{}} }} class Builder {{ {modifier}build(): Product {{ return new Product(); }} call() {{ Builder.build().commit(); }} }}"
        );
        let facts = build_capability_generation(&[("service.ts", &source)], false);
        if modifier.is_empty() {
            assert_abstains(&facts, ("service.ts", "Builder::call", "commit"));
        } else {
            assert_target(
                &facts,
                ("service.ts", "Builder::call", "commit"),
                (
                    "service.ts",
                    "Product::commit",
                    "native-declared-return-receiver",
                ),
            );
        }
    }
}

#[test]
fn types_track_imported_field_preserves_the_real_member_edge() {
    let fixtures = [
        (
            "models.ts",
            "export class TinyCache { remember(key: string): string { return key; } }",
        ),
        (
            "service.ts",
            "import { TinyCache } from './models'; export class UserService { private cache: TinyCache = new TinyCache(); save(label: string): void { this.cache.remember(label); } }",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_target(
        &facts,
        ("service.ts", "UserService::save", "this.cache.remember"),
        (
            "models.ts",
            "TinyCache::remember",
            "native-explicit-receiver-type",
        ),
    );
}

pub(super) fn assert_target(
    facts: &CanonicalGenerationFacts,
    source: (&str, &str, &str),
    expected: (&str, &str, &str),
) {
    let caller = capability_symbol(facts, source.0, source.1);
    let target = capability_symbol(facts, expected.0, expected.1);
    let reference =
        CapabilityReferenceQuery::new(facts, caller).named(source.2, ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{} {}",
        source.0,
        source.2
    );
    assert_eq!(
        reference.resolution_provenance, expected.2,
        "{} {}",
        source.0, source.2
    );
    assert!(facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
        && edge.source_symbol_id == caller.symbol_id
        && edge.target_symbol_id == target.symbol_id
        && edge.provenance == expected.2));
}

#[test]
fn types_and_members_share_parent_constructor_and_return_indexes() {
    let fixtures = [
        (
            "model.dart",
            "class Base { Base(); void save() {} } class Child extends Base { Child(): super(); Child.missing(): super.absent(); void run(Child local) { local.save(); } }",
        ),
        (
            "model.ts",
            "class Product { commit() {} } class Builder { build(): Product { return new Product(); } call(value: Builder) { value.build().commit(); } untyped(value: any) { value.build().commit(); } }",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_eq!(
        facts.digest(),
        build_capability_generation(&fixtures, true).digest()
    );
    for (source, expected) in [
        (
            ("model.dart", "Child::Child", "super"),
            (
                "model.dart",
                "Base::Base",
                "native-dart-constructor-redirect",
            ),
        ),
        (
            ("model.dart", "Child::run", "local.save"),
            ("model.dart", "Base::save", "native-inherited-receiver-type"),
        ),
        (
            ("model.ts", "Builder::call", "commit"),
            (
                "model.ts",
                "Product::commit",
                "native-declared-return-receiver",
            ),
        ),
    ] {
        assert_target(&facts, source, expected);
    }
    for source in [
        ("model.dart", "Child::missing", "super.absent"),
        ("model.ts", "Builder::untyped", "commit"),
    ] {
        let owner = capability_symbol(&facts, source.0, source.1);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(source.2, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    }
}

#[test]
fn javascript_constructor_locals_retain_existing_dynamic_write_fences() {
    let declarations = "class Repo { save() {} } class Other { save() {} }";
    let run = "function run() { const value = new Repo(); value.save(); }";
    for write in ["eval('Repo = Other');", "with (unknown) {}"] {
        let source = format!("{declarations} {write} {run}");
        let facts = build_capability_generation(&[("test.js", &source)], false);
        let owner = capability_symbol(&facts, "test.js", "run");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("value.save", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{write}: {reference:?}"
        );
    }
    let source = format!("{declarations} {run}");
    let facts = build_capability_generation(&[("test.js", &source)], false);
    assert_target(
        &facts,
        ("test.js", "run", "value.save"),
        ("test.js", "Repo::save", "native-explicit-receiver-type"),
    );
}
