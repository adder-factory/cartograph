use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, ReferenceInput, ReferenceKind,
    SymbolInput, build, build_capability_generation, capability_symbol, fs, tempdir,
};
use std::fmt::Write as _;

fn assert_base_field(reference: &ReferenceInput, target: &SymbolInput) {
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, "native-exact-lexical");
    assert_eq!(reference.confidence, 1.0);
}

fn assert_receiver_target(
    facts: &CanonicalGenerationFacts,
    names: (&str, &str, &str),
    target: (&str, &str),
) {
    let caller = capability_symbol(facts, names.0, names.1);
    let member = capability_symbol(facts, target.0, target.1);
    let reference =
        CapabilityReferenceQuery::new(facts, caller).named(names.2, ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&member.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-explicit-receiver-type"
    );
    assert_eq!(reference.confidence, 0.95);
    assert!(facts.edges().iter().any(|edge| {
        edge.source_symbol_id == caller.symbol_id
            && edge.target_symbol_id == member.symbol_id
            && edge.kind == EdgeKind::Calls
            && edge.provenance == reference.resolution_provenance
    }));
}

#[test]
fn explicit_python_receivers_bind_imported_parameters_fields_and_constructors() {
    let fixtures = [
        ("app/models.py", "class Store:\n    def save(self): pass\n"),
        (
            "app/service.py",
            "from app.models import Store as Repo\nclass Service:\n    repo: Repo\n    def call(self, other: Repo):\n        self.repo.save()\n        other.save()\ndef run():\n    service = Service()\n    service.call(Repo())\n",
        ),
        ("unrelated.py", "class Store:\n    def save(self): pass\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_eq!(
        facts.digest(),
        build_capability_generation(&fixtures, true).digest()
    );
    for name in ["self.repo.save", "other.save"] {
        assert_receiver_target(
            &facts,
            ("app/service.py", "Service::call", name),
            ("app/models.py", "Store::save"),
        );
    }
    assert_receiver_target(
        &facts,
        ("app/service.py", "run", "service.call"),
        ("app/service.py", "Service::call"),
    );
}

#[test]
fn explicit_python_receivers_abstain_on_closer_binds_reassignment_and_return_types() {
    let fixtures = [
        ("app/models.py", "class Store:\n    def save(self): pass\n"),
        (
            "app/service.py",
            "from app.models import Store\ndef shadow(value: Store):\n    def inner(value):\n        value.save()\n    return inner\ndef rebind(value: Store):\n    value = unknown()\n    value.save()\ndef factory() -> Store:\n    return Store()\ndef inferred():\n    value = factory()\n    value.save()\ndef type_shadow(Store):\n    value = Store()\n    value.save()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for caller_name in ["shadow::inner", "rebind", "inferred", "type_shadow"] {
        let caller = capability_symbol(&facts, "app/service.py", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{caller_name}");
    }
}

#[test]
fn explicit_python_receiver_types_keep_the_declaration_binding() {
    let fixtures = [
        ("app/models.py", "class Repo:\n    def save(self): pass\n"),
        (
            "app/service.py",
            "from app.models import Repo\nclass Service:\n    repo: Repo\n    def call(self, value: Repo):\n        class Repo:\n            def save(self): pass\n        self.repo.save()\n        value.save()\ndef ambiguous(value: Repo):\n    value.save()\nfrom unknown import Repo\n",
        ),
    ];
    // Two file imports make the import binding ambiguous, even when an unrelated
    // method-local class has the same name. Neither use may pick that local class.
    let facts = build_capability_generation(&fixtures, false);
    for name in ["self.repo.save", "value.save"] {
        let caller = capability_symbol(&facts, "app/service.py", "Service::call");
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none());
    }
    let fixtures = [
        fixtures[0],
        (
            "app/service.py",
            "from app.models import Repo\nclass Service:\n    repo: Repo\n    def call(self, value: Repo):\n        class Repo:\n            def save(self): pass\n        self.repo.save()\n        value.save()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for name in ["self.repo.save", "value.save"] {
        assert_receiver_target(
            &facts,
            ("app/service.py", "Service::call", name),
            ("app/models.py", "Repo::save"),
        );
    }
}

#[test]
fn inherited_receiver_bases_do_not_bind_classes_inside_the_derived_body() {
    let source = "class Base:\n    def describe(self): pass\nclass Child(Base):\n    class Base:\n        def describe(self): pass\n    def call(self):\n        self.describe()\nclass Outer:\n    class Nested(Base):\n        def call(self):\n            self.describe()\n";
    let facts = build_capability_generation(&[("src/model.py", source)], false);
    let caller = capability_symbol(&facts, "src/model.py", "Child::call");
    let target = capability_symbol(&facts, "src/model.py", "Base::describe");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("self.describe", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-inherited-receiver-type"
    );
    let caller = capability_symbol(&facts, "src/model.py", "Outer::Nested::call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("self.describe", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn inherited_receiver_calls_abstain_when_a_closer_field_hides_the_method() {
    let source = "class Base:\n    def run(self): pass\nclass Child(Base):\n    run: int = 0\n    def call(self):\n        self.run()\n";
    let facts = build_capability_generation(&[("src/model.py", source)], false);
    let caller = capability_symbol(&facts, "src/model.py", "Child::call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("self.run", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn inherited_receiver_calls_abstain_on_unproven_base_bindings_and_missing_field_symbols() {
    let fixtures = [
        ("base.py", "class Base:\n    def run(self): pass\n"),
        (
            "child.py",
            "from base import Base\nclass Child(Base):\n    run: int = 0\n",
        ),
        (
            "main.py",
            "from base import Base, Base as Known\nfrom child import Child\ndef read(value: Child):\n    value.run()\nclass Mixed(factory(), Known):\n    def call(self):\n        self.run()\nclass Forward(Later):\n    def call(self):\n        self.run()\nclass Later:\n    def run(self): pass\nBase = unknown()\nclass Shadowed(Base):\n    def call(self):\n        self.run()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for (caller_name, name) in [
        ("read", "value.run"),
        ("Mixed::call", "self.run"),
        ("Forward::call", "self.run"),
        ("Shadowed::call", "self.run"),
    ] {
        let caller = capability_symbol(&facts, "main.py", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{caller_name}");
    }
}

#[test]
fn explicit_receiver_types_do_not_upgrade_heuristic_imports_to_exact_evidence() {
    let fixtures = [
        ("src/app/__init__.py", ""),
        (
            "src/app/models.py",
            "class Store:\n    def save(self): pass\n",
        ),
        (
            "main.py",
            "from app.models import Store\ndef call(value: Store):\n    value.save()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let caller = capability_symbol(&facts, "main.py", "call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn inherited_python_receivers_use_proven_ancestors_and_keep_ambiguity_unresolved() {
    let fixtures = [
        (
            "app/base.py",
            "class Base:\n    def describe(self): pass\nclass Other:\n    def describe(self): pass\n",
        ),
        (
            "app/model.py",
            "from app.base import Base, Other\nclass User(Base):\n    def label(self):\n        return self.describe()\nclass Ambiguous(Base, Other):\n    def label(self):\n        return self.describe()\nclass Unknown(Missing):\n    def label(self):\n        return self.describe()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_eq!(
        facts.digest(),
        build_capability_generation(&fixtures, true).digest()
    );
    let caller = capability_symbol(&facts, "app/model.py", "User::label");
    let member = capability_symbol(&facts, "app/base.py", "Base::describe");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("self.describe", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&member.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-inherited-receiver-type"
    );
    assert_eq!(reference.confidence, 0.9);
    for name in ["Ambiguous::label", "Unknown::label"] {
        let caller = capability_symbol(&facts, "app/model.py", name);
        let reference = CapabilityReferenceQuery::new(&facts, caller)
            .named("self.describe", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
}

#[test]
fn explicit_go_receivers_bind_parameters_fields_and_composite_literals() {
    let source = "package demo\ntype Store interface { Save() }\ntype Service struct { store Store; ID int }\nfunc (s *Service) Call() { s.store.Save(); _ = s.ID }\nfunc (s *Service) Save() {}\nfunc run(s *Service) { s.Save(); local := &Service{}; local.Save(); _ = local.ID }\n";
    let facts = build_capability_generation(&[("src/main.go", source)], false);
    for (caller, name) in [
        ("Service::Call", "s.store.Save"),
        ("run", "s.Save"),
        ("run", "local.Save"),
    ] {
        let target = if name == "s.store.Save" {
            "Store::Save"
        } else {
            "Service::Save"
        };
        assert_receiver_target(
            &facts,
            ("src/main.go", caller, name),
            ("src/main.go", target),
        );
    }
    let member = capability_symbol(&facts, "src/main.go", "Service::ID");
    for (caller_name, provenance, confidence) in [
        ("Service::Call", "native-exact-lexical", 1.0),
        ("run", "native-explicit-receiver-type", 0.95),
    ] {
        let caller = capability_symbol(&facts, "src/main.go", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("ID", ReferenceKind::FieldAccess);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&member.symbol_id));
        assert_eq!(reference.resolution_provenance, provenance);
        assert_eq!(reference.confidence, confidence);
    }
}

#[test]
fn explicit_go_receivers_abstain_on_shadowed_locals_and_factories() {
    let source = "package demo\ntype Store struct {}\nfunc (s *Store) Save() {}\nfunc factory() *Store { return &Store{} }\nfunc shadow(s *Store) { { s := factory(); s.Save() } }\nfunc unknown(s interface{}) { s.Save() }\n";
    let facts = build_capability_generation(&[("src/main.go", source)], false);
    for caller_name in ["shadow", "unknown"] {
        let caller = capability_symbol(&facts, "src/main.go", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("s.Save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{caller_name}");
    }
}

#[test]
fn explicit_this_fields_do_not_bind_same_named_constructor_parameters() {
    let fixtures = [(
        "src/service.ts",
        "class Store {}\nexport class Service { repo: Store; constructor(repo: Store) { this.repo = repo; function nested(repo: Store) { this.repo = repo; } } }",
    )];
    let facts = build_capability_generation(&fixtures, false);
    let caller = capability_symbol(&facts, "src/service.ts", "Service::constructor");
    let target = capability_symbol(&facts, "src/service.ts", "Service::repo");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("repo", ReferenceKind::FieldAccess);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-explicit-receiver-type"
    );
    let nested = capability_symbol(&facts, "src/service.ts", "Service::constructor::nested");
    let reference =
        CapabilityReferenceQuery::new(&facts, nested).named("repo", ReferenceKind::FieldAccess);
    let parameter = capability_symbol(
        &facts,
        "src/service.ts",
        "Service::constructor::nested::repo",
    );
    assert_base_field(reference, parameter);
}

#[test]
fn explicit_receiver_field_reads_never_guess_a_field_on_a_namesake_class() {
    let source = "package demo\ntype Known struct { ID int }\ntype Other struct { ID int }\nfunc run(value interface{}) { _ = value.ID }\n";
    let facts = build_capability_generation(&[("src/main.go", source)], false);
    let caller = capability_symbol(&facts, "src/main.go", "run");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("ID", ReferenceKind::FieldAccess);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn unsupported_this_contexts_preserve_base_field_results() {
    let source = "class Store {}\nexport class Service { repo: Store; static read() { return this.repo; } call() { const obj = { read() { return this.repo; } }; const Other = class { read() { return this.repo; } }; return obj; } }";
    let facts = build_capability_generation(&[("src/service.ts", source)], false);
    let references = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.reference_name == "repo" && reference.reference_kind == "field_access"
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 3);
    let target = capability_symbol(&facts, "src/service.ts", "Service::repo");
    for reference in references {
        assert_base_field(reference, target);
    }
}

#[test]
fn explicit_python_receivers_abstain_on_variadic_and_context_manager_shadows() {
    let source = "class Store:\n    def save(self): pass\ndef outer(value: Store):\n    def inner(*value):\n        value.save()\n    return inner\ndef contexts(value: Store):\n    with unknown() as value:\n        value.save()\n";
    let facts = build_capability_generation(&[("src/service.py", source)], false);
    for name in ["outer::inner", "contexts"] {
        let caller = capability_symbol(&facts, "src/service.py", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none());
    }
}

#[test]
fn inherited_receivers_obey_the_ancestry_bound_and_cycles_abstain() {
    for depth in [12, 35] {
        let mut source = String::from("class Base:\n    def run(self): pass\n");
        for index in 0..depth {
            let parent = if index == 0 {
                "Base".to_owned()
            } else {
                format!("C{}", index - 1)
            };
            writeln!(&mut source, "class C{index}({parent}):\n    pass")
                .unwrap_or_else(|_| panic!("test source allocation"));
        }
        writeln!(
            &mut source,
            "class Caller(C{}):\n    def call(self):\n        self.run()",
            depth - 1
        )
        .unwrap_or_else(|_| panic!("test source allocation"));
        let facts = build_capability_generation(&[("src/service.py", &source)], false);
        let caller = capability_symbol(&facts, "src/service.py", "Caller::call");
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("self.run", ReferenceKind::Calls);
        if depth == 12 {
            let target = capability_symbol(&facts, "src/service.py", "Base::run");
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        } else {
            assert!(reference.target_symbol_id.is_none());
        }
    }
    let source = "class A(B):\n    def call(self):\n        self.run()\nclass B(A): pass\nclass Other:\n    def run(self): pass\n";
    let facts = build_capability_generation(&[("src/service.py", source)], false);
    let caller = capability_symbol(&facts, "src/service.py", "A::call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("self.run", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_receiver_targets_are_worker_invariant() {
    let directory = tempdir().unwrap_or_else(|error| panic!("receiver fixture: {error}"));
    let fixtures = [
        ("base.py", "class Parent:\n    def send(self): pass\n"),
        (
            "child.py",
            "from base import Parent\nclass Child(Parent):\n    def call(self, other: Parent):\n        self.send()\n        other.send()\n",
        ),
        (
            "main.go",
            "package demo\ntype Queue struct {}\nfunc (q *Queue) Send() {}\nfunc call(q *Queue) { q.Send() }\n",
        ),
    ];
    for (path, source) in fixtures {
        fs::write(directory.path().join(path), source)
            .unwrap_or_else(|error| panic!("receiver fixture write: {error}"));
    }
    let serial = build(directory.path(), 1).await;
    assert_receiver_target(
        serial.facts(),
        ("child.py", "Child::call", "other.send"),
        ("base.py", "Parent::send"),
    );
    for workers in [2, 4, 8, 16] {
        let parallel = build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers} workers"
        );
    }
}

#[test]
fn explicit_python_receiver_types_abstain_after_wildcards_and_type_aliases() {
    let fixtures = [
        ("models.py", "class Store:\n    def save(self): pass\n"),
        ("other.py", "class Store:\n    def save(self): pass\n"),
        (
            "wildcard.py",
            "from models import Store\nfrom other import *\ndef call(value: Store):\n    value.save()\n",
        ),
        (
            "alias.py",
            "from models import Store\ntype Store = int\ndef call(value: Store):\n    value.save()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for path in ["wildcard.py", "alias.py"] {
        let caller = capability_symbol(&facts, path, "call");
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{path}");
    }
}

#[test]
fn unsupported_static_contexts_preserve_base_field_results() {
    let source =
        "class Service { repo = 1; static snapshot = this.repo; static { consume(this.repo); } }";
    let fixtures = [("service.js", source), ("service.ts", source)];
    let facts = build_capability_generation(&fixtures, false);
    let references = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.reference_name == "repo" && reference.reference_kind == "field_access"
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 4);
    for path in ["service.js", "service.ts"] {
        let target = capability_symbol(&facts, path, "Service::repo");
        for reference in references
            .iter()
            .filter(|reference| reference.file_id == target.file_id)
        {
            assert_base_field(reference, target);
        }
    }
}

#[test]
fn explicit_constructor_receivers_abstain_before_local_type_and_import_bindings() {
    let fixtures = [
        ("models.py", "class Store:\n    def save(self): pass\n"),
        (
            "main.py",
            "def local_type():\n    value = Repo()\n    class Repo:\n        def save(self): pass\n    value.save()\ndef local_import():\n    value = Store()\n    from models import Store\n    value.save()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for name in ["local_type", "local_import"] {
        let caller = capability_symbol(&facts, "main.py", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
}

fn go_shadow_generation(body: &str) -> CanonicalGenerationFacts {
    let source = format!(
        "package demo\ntype Store struct {{}}\nfunc (s *Store) Save() {{}}\ntype Other struct {{}}\nfunc (s *Other) Save() {{}}\n{body}\n"
    );
    build_capability_generation(&[("main.go", &source)], false)
}

#[test]
fn explicit_go_receivers_abstain_on_type_switch_aliases() {
    let facts = go_shadow_generation(
        "func call(x *Store, value interface{}) { switch x := value.(type) { case *Other: x.Save() } }",
    );
    let caller = capability_symbol(&facts, "main.go", "call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("x.Save", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn explicit_go_receiver_declarations_do_not_apply_before_their_binding() {
    let facts = go_shadow_generation("func call(x *Store) { { x.Save(); var x *Other; _ = x } }");
    let caller = capability_symbol(&facts, "main.go", "call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("x.Save", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn explicit_variadic_receivers_do_not_use_the_element_type() {
    let facts = go_shadow_generation("func call(values ...Store) { values.Save() }");
    let caller = capability_symbol(&facts, "main.go", "call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("values.Save", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
    let source = "class Store:\n    def save(self): pass\ndef outer(value: Store):\n    def inner(*value: Store):\n        value.save()\n    return inner\n";
    let facts = build_capability_generation(&[("main.py", source)], false);
    let caller = capability_symbol(&facts, "main.py", "outer::inner");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn inherited_receivers_keep_field_blockers_after_an_unsupported_class_binding() {
    let fixtures = [
        ("base.py", "class Base:\n    def run(self): pass\n"),
        (
            "child.py",
            "from base import Base\nclass Child(Base):\n    run: int = 0\n    type Alias = int\n",
        ),
        (
            "main.py",
            "from child import Child\ndef call(value: Child):\n    value.run()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let caller = capability_symbol(&facts, "main.py", "call");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("value.run", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn explicit_class_binding_fences_are_preserved_for_cross_file_receivers() {
    for declarations in [
        "    type run = int\n",
        "    def run(self): pass\n    type run = int\n",
    ] {
        let child = format!("from base import Base\nclass Child(Base):\n{declarations}");
        let fixtures = [
            ("base.py", "class Base:\n    def run(self): pass\n"),
            ("child.py", child.as_str()),
            (
                "main.py",
                "from child import Child\ndef call(value: Child):\n    value.run()\n",
            ),
        ];
        let facts = build_capability_generation(&fixtures, false);
        let caller = capability_symbol(&facts, "main.py", "call");
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.run", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{declarations}");
    }
}

#[test]
fn explicit_python_receivers_abstain_when_name_mangling_is_unproven() {
    let fixtures = [
        (
            "base.py",
            "class Base:\n    def __run(self): pass\n    def __call__(self): pass\n",
        ),
        (
            "main.py",
            "from base import Base\nclass Child(Base):\n    def call(self):\n        self.__run()\ndef outside(value: Base):\n    value.__run()\ndef dunder(value: Base):\n    value.__call__()\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    for (caller_name, name) in [("Child::call", "self.__run"), ("outside", "value.__run")] {
        let caller = capability_symbol(&facts, "main.py", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{caller_name}");
    }
    assert_receiver_target(
        &facts,
        ("main.py", "dunder", "value.__call__"),
        ("base.py", "Base::__call__"),
    );
}

#[test]
fn excluded_static_members_preserve_base_field_results() {
    let source = "class Service { static repo = 1; read() { return this.repo; } }";
    let facts =
        build_capability_generation(&[("service.js", source), ("service.ts", source)], false);
    let references = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.reference_name == "repo" && reference.reference_kind == "field_access"
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 2);
    for path in ["service.js", "service.ts"] {
        let target = capability_symbol(&facts, path, "Service::repo");
        for reference in references
            .iter()
            .filter(|reference| reference.file_id == target.file_id)
        {
            assert_base_field(reference, target);
        }
    }
}

#[test]
fn unsupported_explicit_this_parameters_preserve_base_field_results() {
    let source = "class Other { repo = 1; } class Service { repo = 2; read(this: Other) { return this.repo; } }";
    let facts = build_capability_generation(&[("service.ts", source)], false);
    let caller = capability_symbol(&facts, "service.ts", "Service::read");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("repo", ReferenceKind::FieldAccess);
    let target = capability_symbol(&facts, "service.ts", "Service::repo");
    assert_base_field(reference, target);
}

#[test]
fn python_generic_scopes_never_bind_a_type_parameter_to_a_module_class() {
    let declarations =
        "class Store:\n    def save(self): pass\nclass Other:\n    def save(self): pass\n";
    for (body, name) in [
        (
            "def call[Store](value: Store):\n    value.save()\ncall(Other())\n",
            "call",
        ),
        (
            "class C[Store]:\n    def read(self, value: Store):\n        value.save()\n",
            "C::read",
        ),
        (
            "type X[Store] = Store\ndef alias(value: X):\n    value.save()\n",
            "alias",
        ),
    ] {
        let source = format!("{declarations}{body}");
        let facts = build_capability_generation(&[("main.py", &source)], false);
        let caller = capability_symbol(&facts, "main.py", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("value.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
}

#[test]
fn python_member_assignments_fence_direct_and_inherited_receiver_calls() {
    let source = "def replacement(): pass\nclass Base:\n    def save(self): pass\nclass Child(Base):\n    def __init__(self):\n        self.save = replacement\n    def call(self):\n        self.save()\nclass Direct:\n    def save(self): pass\n    def change(self, obj: Direct):\n        obj.save = replacement\n    def call(self):\n        self.save()\n";
    let facts = build_capability_generation(&[("main.py", source)], false);
    for name in ["Child::call", "Direct::call"] {
        let caller = capability_symbol(&facts, "main.py", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, caller).named("self.save", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
}

#[test]
fn python_imported_receiver_assignments_abstain_without_losing_unassigned_calls() {
    for assigned in [false, true] {
        let assignment = if assigned {
            "    x.save = replacement\n"
        } else {
            ""
        };
        let source = format!(
            "from store import Store\ndef replacement(): pass\ndef call(x: Store):\n{assignment}    x.save()\n"
        );
        let fixtures = [
            ("store.py", "class Store:\n    def save(self): pass\n"),
            ("main.py", source.as_str()),
        ];
        let facts = build_capability_generation(&fixtures, false);
        if assigned {
            let caller = capability_symbol(&facts, "main.py", "call");
            let reference =
                CapabilityReferenceQuery::new(&facts, caller).named("x.save", ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none());
        } else {
            assert_receiver_target(
                &facts,
                ("main.py", "call", "x.save"),
                ("store.py", "Store::save"),
            );
        }
    }
}

fn assert_exact_field_metadata(source: &str, caller_name: &str) {
    let facts = build_capability_generation(&[("service.js", source)], false);
    let caller = capability_symbol(&facts, "service.js", caller_name);
    let target = capability_symbol(&facts, "service.js", "Service::repo");
    let reference =
        CapabilityReferenceQuery::new(&facts, caller).named("repo", ReferenceKind::FieldAccess);
    assert_base_field(reference, target);
}

#[test]
fn unknown_static_receiver_evidence_preserves_the_base_field_resolution() {
    assert_exact_field_metadata(
        "class Service { static repo = 1; static read() { return this.repo; } }",
        "Service::read",
    );
}

#[test]
fn agreeing_instance_receiver_evidence_preserves_the_base_field_metadata() {
    assert_exact_field_metadata(
        "class Service { repo = 1; read() { return this.repo; } }",
        "Service::read",
    );
}
