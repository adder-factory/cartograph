//! Unhandled binders must never reuse a namesake project class or outer value.

use super::{build_capability_generation, types_track::assert_abstains};
use cartograph_extract::{
    EXPLICIT_RECEIVER_RESOLUTION_PREFIX, NativeExtractor, SourceLimits, SourceSnapshot,
};

const QUALIFIED_ALIAS_SOURCES: &[&str] = &[
    "namespace N { class Repo { public: void save() {} }; } struct Other { struct Repo { void save() {} }; }; void run() { using N = Other; N::Repo param; param.save(); }",
    "namespace N { class Repo { public: void save() {} }; } namespace Other { struct Repo { void save() {} }; } void run() { namespace N = Other; N::Repo param; param.save(); }",
];

const RUBY_OVERRIDE_SOURCES: &[&str] = &[
    "class Other\n def save; 'other'; end\nend\nclass Base\n def self.new; Other.new; end\nend\nclass Repo < Base\n def save; 'repo'; end\nend\n",
    "class Other\n def save; 'other'; end\nend\nclass Repo\n def save; 'repo'; end\nend\ndef Repo.new; Other.new; end\n",
    "class Other\n def save; 'other'; end\nend\nclass Repo\n def save; 'repo'; end\nend\nclass << Repo\n def new; Other.new; end\nend\n",
];

const KOTLIN_INVOKE_CLASSES: &[&str] = &[
    "object Repo {\n operator fun invoke(value: Int): Other {\n return Other()\n }\n fun save() {}\n}\n",
    "class Repo {\n fun save() {}\n companion object {\n operator fun invoke(value: Int): Other {\n return Other()\n }\n }\n}\n",
];

const KOTLIN_DEFAULT_FACTORIES: &[(&str, &str)] = &[
    (
        "class arrayOf {\n override fun toString(): String = \"repo\"\n}\n",
        "arrayOf(1)",
    ),
    (
        "class emptyList {\n override fun toString(): String = \"repo\"\n}\n",
        "emptyList<Int>()",
    ),
];

const ALIAS_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "test.rb",
        "class Other\n def save; 'other'; end\nend\nclass Repo\n def save; 'repo'; end\n def self.new; Other.new; end\nend\ndef run\n local = Repo.new\n local.save\nend\n",
        "run",
        "local.save",
    ),
    (
        "test.rb",
        "class Other\n def save; 'other'; end\nend\nclass Repo\n def save; 'repo'; end\n def self.new; Other.new; end\nend\ndef run\n Repo.new.save\nend\n",
        "run",
        "Repo.save",
    ),
    (
        "test.kt",
        "import kotlin.math.abs as Repo\nclass Repo {\n override fun toString(): String = \"repo\"\n}\nfun run() {\n val local = Repo(-1)\n local.toString()\n}\n",
        "run",
        "local.toString",
    ),
    (
        "test.cpp",
        "class Repo { public: void save() {} }; class Other { public: void save() {} }; void run() { using Repo = Other; Repo param; param.save(); }",
        "run",
        "param.save",
    ),
    (
        "test.cpp",
        "class Repo { public: void save() {} }; class Other { public: void save() {} }; void run() { typedef Other Repo; Repo param; param.save(); }",
        "run",
        "param.save",
    ),
    (
        "test.m",
        "@interface Repo\n- (void)save;\n@end\n@interface Other\n- (void)save;\n@end\nvoid run(void) { typedef Other *Repo; Repo param; [param save]; }",
        "run",
        "param.save",
    ),
    (
        "test.cs",
        "public class Repo { public void Save() {} } public class Other { public void Save() {} } namespace Client { using Repo = global::Other; class Service { void Run(Repo param) { param.Save(); } } }",
        "Client::Service::Run",
        "param.Save",
    ),
    (
        "test.java",
        "class Repo { void save() {} } class Other { void save() {} } class Service { enum Repo { ONE; void save() {} } void run(Repo param) { param.save(); } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.swift",
        "class Repo { func save() {} }\nclass Service { enum Repo { case one; func save() {} }\nfunc run(param: Repo) { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.kt",
        "class Repo {\n fun save() {}\n}\nclass Service {\n enum class Repo {\n ONE;\n fun save() {}\n }\n fun run(param: Repo) {\n param.save()\n }\n}\n",
        "Service::run",
        "param.save",
    ),
    (
        "test.scala",
        "class Repo { def save(): Unit = {} }\nclass Service { enum Repo { case One; def save(): Unit = {} }\ndef run(param: Repo): Unit = { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.scala",
        "class Repo { def save(): Unit = {} }\nclass Service { trait Repo { def save(): Unit }; def run(param: Repo): Unit = { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.ts",
        "class Repo { toString() { return ''; } } function run() { enum Repo { One } let param: Repo = Repo.One; param.toString(); }",
        "run",
        "param.toString",
    ),
    (
        "test.pas",
        "unit Test; interface type TRepo = class procedure Save(); end; TOther = class procedure Save(); end; implementation procedure Run(); type TRepo = TOther; var param: TRepo; begin param.Save(); end; end.",
        "Run",
        "param.Save",
    ),
    (
        "test.pas",
        "unit Test; interface type TRepo = class procedure Save(); end; generic TBox<TRepo> = class procedure Run(param: TRepo); end; implementation procedure TBox.Run(param: TRepo); begin param.Save(); end; end.",
        "TBox::Run",
        "param.Save",
    ),
    (
        "test.swift",
        "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { typealias Repo = Other\nfunc run(param: Repo) { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.scala",
        "class Repo { def save(): Unit = {} }\nclass Other { def save(): Unit = {} }\nclass Service { type Repo = Other\ndef run(param: Repo): Unit = { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.kt",
        "class Repo {\n fun save() {}\n}\nclass Other {\n fun save() {}\n}\ntypealias Alias = Other\nclass Service<Repo> {\n fun run(param: Repo) {\n param.save()\n }\n}\n",
        "Service::run",
        "param.save",
    ),
    (
        "test.ts",
        "class Repo { save() {} } class Other { save() {} } function run() { type Repo = Other; let param: Repo; param.save(); }",
        "run",
        "param.save",
    ),
    (
        "test.scala",
        "class Repo { def save(): Unit = {} }\nclass Service[Repo] { def run(param: Repo): Unit = { param.save() } }",
        "Service::run",
        "param.save",
    ),
    (
        "test.dart",
        "class Repo { void save() {} } class Service<Repo> { void run(Repo param) { param.save(); } }",
        "Service::run",
        "param.save",
    ),
];

#[test]
fn aliases_and_generic_parameters_do_not_prove_project_namesakes() {
    for &(path, source, caller, name) in ALIAS_CASES {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_parsed(&facts, path);
        assert_abstains(&facts, (path, caller, name));
    }
}

#[test]
fn kotlin_imported_callables_cannot_supply_constructor_proof() {
    let imports = ["import factories.Repo", "import factories.*"];
    for import in imports {
        let source = format!(
            "{import}\nclass Repo {{\n override fun toString(): String = \"repo\"\n}}\nfun run() {{\n val local = Repo(-1)\n local.toString()\n}}\n"
        );
        let fixtures = [
            ("test.kt", source.as_str()),
            (
                "factories.kt",
                "package factories\nfun Repo(value: Int): Int = value\n",
            ),
        ];
        let facts = build_capability_generation(&fixtures, false);
        assert_parsed(&facts, "test.kt");
        assert_abstains(&facts, ("test.kt", "run", "local.toString"));
    }
}

#[test]
fn kotlin_root_imports_cannot_supply_constructor_proof() {
    let imports = ["import factory as Repo", "import Repo"];
    for import in imports {
        let source = format!(
            "package client\n{import}\nclass Repo {{\n override fun toString(): String = \"repo\"\n}}\nfun run() {{\n val local = Repo(1)\n local.toString()\n}}\n"
        );
        let fixtures = [
            ("test.kt", source.as_str()),
            (
                "factories.kt",
                "fun factory(value: Int): Int = value\nfun Repo(value: Int): Int = value\n",
            ),
        ];
        let facts = build_capability_generation(&fixtures, false);
        assert_parsed(&facts, "test.kt");
        assert_abstains(&facts, ("test.kt", "client::run", "local.toString"));
    }
}

#[test]
fn kotlin_invoke_factories_preserve_annotated_receivers() {
    for declaration in KOTLIN_INVOKE_CLASSES {
        let source = format!(
            "class Other {{\n fun save() {{}}\n}}\n{declaration}\nfun run(param: Repo) {{\n val local = Repo(1)\n local.save()\n param.save()\n}}\n"
        );
        let facts = build_capability_generation(&[("test.kt", source.as_str())], false);
        assert_parsed(&facts, "test.kt");
        assert_abstains(&facts, ("test.kt", "run", "local.save"));
        super::types_track::assert_target(
            &facts,
            ("test.kt", "run", "param.save"),
            ("test.kt", "Repo::save", "native-explicit-receiver-type"),
        );
    }
}

#[test]
fn kotlin_bare_calls_require_default_constructor_evidence() {
    for (declaration, initializer) in KOTLIN_DEFAULT_FACTORIES {
        let source = format!(
            "{declaration}\nfun run() {{\n val local = {initializer}\n local.toString()\n}}\n"
        );
        let facts = build_capability_generation(&[("test.kt", source.as_str())], false);
        assert_parsed(&facts, "test.kt");
        assert_abstains(&facts, ("test.kt", "run", "local.toString"));
    }
}

#[test]
fn kotlin_imported_factory_cannot_bind_a_class_in_another_file() {
    let fixtures = [
        (
            "test.kt",
            "import factories.Repo\nfun run() {\n val local = Repo(-1)\n local.toString()\n}\n",
        ),
        (
            "factories.kt",
            "package factories\nfun Repo(value: Int): Int = value\n",
        ),
        (
            "repo.kt",
            "class Repo {\n override fun toString(): String = \"repo\"\n}\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_parsed(&facts, "test.kt");
    assert_abstains(&facts, ("test.kt", "run", "local.toString"));
}

#[test]
fn ruby_singleton_factories_cannot_supply_constructor_proof() {
    for source in RUBY_OVERRIDE_SOURCES {
        let source =
            format!("{source}\ndef run\n local = Repo.new\n local.save\n Repo.new.save\nend\n");
        let facts = build_capability_generation(&[("test.rb", source.as_str())], false);
        assert_parsed(&facts, "test.rb");
        assert_abstains(&facts, ("test.rb", "run", "local.save"));
        assert_abstains(&facts, ("test.rb", "run", "Repo.save"));
    }
}

#[test]
fn kotlin_explicit_imported_class_annotations_keep_their_real_member() {
    let fixtures = [
        (
            "test.kt",
            "import factories.Repo\nfun run(param: Repo) {\n param.save()\n}\n",
        ),
        (
            "factories.kt",
            "package factories\nclass Repo {\n fun save() {}\n}\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_parsed(&facts, "test.kt");
    super::types_track::assert_target(
        &facts,
        ("test.kt", "run", "param.save"),
        (
            "factories.kt",
            "factories::Repo::save",
            "native-explicit-receiver-type",
        ),
    );
}

#[test]
fn admitted_nested_records_and_objects_keep_their_own_member() {
    let cases = [
        (
            "test.cs",
            "class Repo { public void Save() {} } class Service { record Repo { public void Save() {} } void Run(Repo param) { param.Save(); } }",
            "Service::Run",
            "param.Save",
            "Service::Repo::Save",
        ),
        (
            "test.java",
            "class Repo { void save() {} } class Service { record Repo() { void save() {} } void run(Repo param) { param.save(); } }",
            "Service::run",
            "param.save",
            "Service::Repo::save",
        ),
        (
            "test.kt",
            "class Repo {\n fun save() {}\n}\nclass Service {\n object Repo {\n fun save() {}\n }\n fun run(param: Repo) {\n param.save()\n }\n}\n",
            "Service::run",
            "param.save",
            "Service::Repo::save",
        ),
        (
            "test.ts",
            "class Repo { save() {} } function run() { interface Repo { save(): void; } let param: Repo; param.save(); }",
            "run",
            "param.save",
            "run::Repo::save",
        ),
    ];
    for (path, source, caller, reference, target) in cases {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_parsed(&facts, path);
        super::types_track::assert_target(
            &facts,
            (path, caller, reference),
            (path, target, "native-explicit-receiver-type"),
        );
    }
}

#[test]
fn qualified_aliases_cannot_emit_unshadowed_type_evidence() {
    let limits = SourceLimits::new(super::TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limits: {error}"));
    for source in QUALIFIED_ALIAS_SOURCES {
        let facts = build_capability_generation(&[("test.cpp", source)], false);
        assert_parsed(&facts, "test.cpp");
        assert_abstains(&facts, ("test.cpp", "run", "param.save"));
        let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
            "test.cpp",
            source.as_bytes(),
            limits,
        )
        .unwrap_or_else(|error| panic!("receiver snapshot: {error}"));
        let mut extractor = NativeExtractor::new_for_capability_validation(snapshot.language())
            .unwrap_or_else(|error| panic!("receiver extractor: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("receiver extraction: {error}"));
        let call = extracted
            .references
            .iter()
            .find(|reference| reference.name == "param.save")
            .unwrap_or_else(|| panic!("missing actual receiver call"));
        let lookup = extracted
            .receiver_evidence
            .as_deref()
            .and_then(|evidence| {
                evidence
                    .lookups
                    .iter()
                    .find(|lookup| lookup.span == call.span)
            })
            .unwrap_or_else(|| panic!("missing receiver disposition"));
        assert_eq!(
            lookup.lookup,
            format!("{EXPLICIT_RECEIVER_RESOLUTION_PREFIX}?#save")
        );
    }
}

#[test]
fn catches_and_iteration_bindings_do_not_reuse_outer_types() {
    let cases = [
        (
            "test.cs",
            "class Repo { public void Save() {} } class Other : System.Exception { public void Save() {} } class Service { Repo param; void Run() { try {} catch (Other param) { param.Save(); } } }",
            "Service::Run",
            "param.Save",
        ),
        (
            "test.swift",
            "class Repo { func save() {} }\nclass Service { var param: Repo\nfunc run() { do { try unknown() } catch let param { param.save() } } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.kt",
            "class Repo {\n fun save() {}\n}\nclass Other: Exception() {\n fun save() {}\n}\nclass Service(val param: Repo) {\n fun run() {\n try {\n unknown()\n } catch (param: Other) {\n param.save()\n }\n }\n}\n",
            "Service::run",
            "param.save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Service { Repo param; void run() { try { unknown(); } catch (param) { param.save(); } } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.cs",
            "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { Repo param; void Run() { foreach (var param in others) { param.Save(); } } }",
            "Service::Run",
            "param.Save",
        ),
        (
            "test.cpp",
            "class Repo { public: void save() {} }; class Other { public: void save() {} }; void run(Repo param, Other (&others)[1]) { for (auto param : others) { param.save(); } }",
            "run",
            "param.save",
        ),
        (
            "test.scala",
            "class Repo { def save(): Unit = {} }\nclass Other { def save(): Unit = {} }\nclass Service { def run(param: Repo, others: List[Other]): Unit = { for (param <- others) param.save() } }",
            "Service::run",
            "param.save",
        ),
        (
            "test.ts",
            "class Repo { save() {} } class Other { save() {} } function run(param: Repo) { try { unknown(); } catch (param) { param.save(); } }",
            "run",
            "param.save",
        ),
    ];
    for (path, source, caller, name) in cases {
        let facts = build_capability_generation(&[(path, source)], false);
        assert_parsed(&facts, path);
        assert_abstains(&facts, (path, caller, name));
    }
}

fn assert_parsed(facts: &super::CanonicalGenerationFacts, path: &str) {
    assert!(
        facts
            .files()
            .iter()
            .all(|file| { file.parse_status == cartograph_domain::FileParseStatus::Parsed }),
        "fixture must parse completely: {path}"
    );
}
