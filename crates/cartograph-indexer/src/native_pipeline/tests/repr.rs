use super::*;
use std::fmt::Write as _;

mod health;

async fn generation(fixtures: &[(&str, &str)]) -> NativeGeneration {
    let directory = tempdir().unwrap_or_else(|error| panic!("fixture directory: {error}"));
    for (path, source) in fixtures {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap_or(directory.path()))
            .unwrap_or_else(|error| panic!("fixture parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("fixture source: {error}"));
    }
    let serial = build(directory.path(), SERIAL_WORKERS).await;
    let parallel = build(directory.path(), PARALLEL_WORKERS).await;
    assert_eq!(serial.facts().digest(), parallel.facts().digest());
    assert_eq!(serial.facts().references(), parallel.facts().references());
    assert_eq!(serial.facts().edges(), parallel.facts().edges());
    serial
}

fn site<'a>(
    facts: &'a CanonicalGenerationFacts,
    path: &str,
    (name, kind): (&str, ReferenceKind),
) -> &'a ReferenceInput {
    let file = capability_file_symbol(facts, path);
    facts
        .references()
        .iter()
        .find(|reference| {
            reference.file_id == file.file_id
                && reference.reference_name == name
                && reference.reference_kind == kind.as_str()
        })
        .unwrap_or_else(|| panic!("missing {path}:{name}:{kind:?}"))
}

fn assert_target(
    facts: &CanonicalGenerationFacts,
    reference: &ReferenceInput,
    (path, name, provenance): (&str, &str, &str),
) {
    let target = capability_symbol(facts, path, name);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, provenance);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_imports_target_declarations_and_missing_modules_stay_targetless() {
    let built = generation(&[
        ("impl.ts", "export function original() {}"),
        ("other.ts", "export function absent() {}"),
        (
            "main.ts",
            "import { original as local } from './impl'; import { absent } from './missing'; export function run() { local(); absent(); }",
        ),
    ]).await;
    let facts = built.facts();
    assert_target(
        facts,
        site(facts, "main.ts", ("local", ReferenceKind::Calls)),
        ("impl.ts", "original", IMPORT_BINDING_PROVENANCE),
    );
    assert_target(
        facts,
        site(facts, "main.ts", ("original", ReferenceKind::References)),
        ("impl.ts", "original", IMPORT_BINDING_PROVENANCE),
    );
    assert_target(
        facts,
        site(facts, "main.ts", ("./impl", ReferenceKind::Imports)),
        ("impl.ts", "impl.ts", "native-module-file-path"),
    );
    assert!(
        site(facts, "main.ts", ("absent", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
    assert!(
        site(facts, "main.ts", ("absent", ReferenceKind::References))
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn def_use_targets_the_local_declaration_and_never_a_callable_self_loop() {
    let built = generation(&[
        ("main.ts", "export function use() { let value = 1; return value; } export function unused() { let value = 2; }"),
        ("other.ts", "export const value = 3;"),
    ]).await;
    let facts = built.facts();
    let reference = site(facts, "main.ts", ("value", ReferenceKind::DefUse));
    assert_target(
        facts,
        reference,
        ("main.ts", "use::value", EXACT_LEXICAL_PROVENANCE),
    );
    assert!(facts.edges().iter().all(
        |edge| edge.kind != EdgeKind::DefUse || edge.source_symbol_id != edge.target_symbol_id
    ));
    let unused = capability_symbol(facts, "main.ts", "unused");
    assert!(
        facts
            .references()
            .iter()
            .all(
                |reference| reference.reference_kind != ReferenceKind::DefUse.as_str()
                    || reference.owner_symbol_id.as_ref() != Some(&unused.symbol_id)
            )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_subject_edges_are_derived_between_exact_files_and_orphans_abstain() {
    let built = generation(&[
        ("src/math.ts", "export function add() { return 1; }"),
        ("src/math.test.ts", "test('adds', () => {});"),
        ("src/orphan.test.ts", "test('orphans', () => {});"),
        ("other/unrelated.ts", "export function unrelated() {}"),
    ])
    .await;
    let facts = built.facts();
    let source = capability_file_symbol(facts, "src/math.test.ts");
    let target = capability_file_symbol(facts, "src/math.ts");
    let edge = facts
        .edges()
        .iter()
        .find(|edge| edge.source_symbol_id == source.symbol_id && edge.kind == EdgeKind::Tests)
        .unwrap_or_else(|| panic!("missing exact test subject edge"));
    assert_eq!(edge.target_symbol_id, target.symbol_id);
    assert_eq!(edge.provenance, TEST_CONVENTION_PROVENANCE);
    assert_confidence(edge.confidence, TEST_CONVENTION_CONFIDENCE);
    let orphan = capability_file_symbol(facts, "src/orphan.test.ts");
    assert!(
        facts.edges().iter().all(|edge| {
            edge.kind != EdgeKind::Tests || edge.source_symbol_id != orphan.symbol_id
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn literal_resource_and_side_effect_imports_resolve_to_exact_files() {
    let built = generation(&[
        ("src/main.js", "import '../styles/base.css'; import data from '../config/settings.json'; import './impl.js'; import '../missing/settings.json';"),
        ("styles/base.css", "body { color: red; }"),
        ("config/settings.json", "{}"),
        ("src/impl.js", "export function run() {}"),
    ]).await;
    let facts = built.facts();
    for (name, path) in [
        ("../styles/base.css", "styles/base.css"),
        ("../config/settings.json", "config/settings.json"),
        ("./impl.js", "src/impl.js"),
    ] {
        assert_target(
            facts,
            site(facts, "src/main.js", (name, ReferenceKind::Imports)),
            (path, path, "native-module-file-path"),
        );
    }
    assert!(
        site(
            facts,
            "src/main.js",
            ("../missing/settings.json", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dart_relative_imports_resolve_files_without_external_package_guessing() {
    let built = generation(&[
        (
            "lib/main.dart",
            "import 'screens/screen.dart'; import 'package:missing/screen.dart'; void main() {}",
        ),
        ("lib/screens/screen.dart", "class Screen {}"),
    ])
    .await;
    let facts = built.facts();
    assert_target(
        facts,
        site(
            facts,
            "lib/main.dart",
            ("screens/screen.dart", ReferenceKind::Imports),
        ),
        (
            "lib/screens/screen.dart",
            "lib/screens/screen.dart",
            "native-module-file-path",
        ),
    );
    assert!(
        site(
            facts,
            "lib/main.dart",
            ("package:missing/screen.dart", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liquid_paths_preserve_import_edges_and_suffix_ambiguity() {
    let built = generation(&[
        (
            "theme/layout/theme.liquid",
            "{% render 'unique' %}{% render 'shared' %}",
        ),
        ("theme/snippets/unique.liquid", "text"),
        ("theme/snippets/shared.liquid", "text"),
        ("other/snippets/shared.liquid", "text"),
    ])
    .await;
    let facts = built.facts();
    let reference = site(
        facts,
        "theme/layout/theme.liquid",
        ("snippets/unique.liquid", ReferenceKind::Imports),
    );
    assert_target(
        facts,
        reference,
        (
            "theme/snippets/unique.liquid",
            "theme/snippets/unique.liquid",
            "native-file-path-suffix",
        ),
    );
    assert_confidence(reference.confidence, 0.85);
    assert!(
        site(
            facts,
            "theme/layout/theme.liquid",
            ("snippets/shared.liquid", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_and_default_reexports_bind_calls_to_original_declarations() {
    let built = generation(&[
        ("impl.ts", "export function original() {} export default function primary() {}"),
        ("barrel.ts", "export { original as renamed, default } from './impl';"),
        ("middle.ts", "export { renamed, default } from './barrel';"),
        ("main.ts", "import primary, { renamed } from './middle'; export function run() { renamed(); primary(); }"),
    ]).await;
    let facts = built.facts();
    for (name, target) in [("renamed", "original"), ("primary", "primary")] {
        assert_target(
            facts,
            site(facts, "main.ts", (name, ReferenceKind::Calls)),
            ("impl.ts", target, "native-reexport-alias"),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cyclic_or_missing_reexport_targets_never_bind_to_an_export_landmark() {
    let built = generation(&[
        ("one.ts", "export { renamed } from './two';\nexport { original as absent } from './missing';"),
        ("two.ts", "export { renamed } from './one';"),
        ("other.ts", "export function original() {}"),
        ("duplicate.ts", "export { original as duplicate } from './other';\nexport { original as duplicate } from './missing';"),
        ("main.ts", "import { renamed, absent } from './one'; import { duplicate } from './duplicate'; export function run() { renamed(); absent(); duplicate(); }"),
    ]).await;
    let facts = built.facts();
    for name in ["renamed", "absent", "duplicate"] {
        assert!(
            site(facts, "main.ts", (name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_export_landmarks_preserve_the_existing_import_target() {
    let built = generation(&[
        (
            "impl.ts",
            "function original() {} export { original as inner };",
        ),
        ("barrel.ts", "export { inner as outer } from './impl';"),
        (
            "main.ts",
            "import { outer } from './barrel'; export function run() { outer(); }",
        ),
    ])
    .await;
    let facts = built.facts();
    let reference = site(facts, "main.ts", ("outer", ReferenceKind::Calls));
    assert_target(
        facts,
        reference,
        ("barrel.ts", "outer", IMPORT_BINDING_PROVENANCE),
    );
    assert_confidence(reference.confidence, IMPORT_BINDING_CONFIDENCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_export_group_preserves_all_exact_targets_and_existing_density_limits() {
    const EXPORT_COUNT: usize = 128;
    const DENSE_EXPORT_COUNT: usize = 4_096;
    const MIDDLE_EXPORT: usize = EXPORT_COUNT / 2;
    let mut implementation = String::new();
    let mut names = Vec::new();
    for position in 0..EXPORT_COUNT {
        writeln!(implementation, "export function a{position}() {{}}")
            .unwrap_or_else(|error| panic!("fixture source: {error}"));
        names.push(format!("a{position}"));
    }
    let barrel = format!("export {{ {} }} from './impl';", names.join(", "));
    let main = format!(
        "import {{ a0, a{MIDDLE_EXPORT}, a{} }} from './barrel'; export function run() {{ a0(); a{MIDDLE_EXPORT}(); a{}(); }}",
        EXPORT_COUNT - 1,
        EXPORT_COUNT - 1
    );
    let built = generation(&[
        ("impl.ts", &implementation),
        ("barrel.ts", &barrel),
        ("main.ts", &main),
    ])
    .await;
    let facts = built.facts();
    let file = capability_file_symbol(facts, "barrel.ts");
    assert_eq!(
        facts
            .symbols()
            .iter()
            .filter(|symbol| symbol.file_id == file.file_id
                && symbol.symbol_kind == SymbolKind::Export.as_str())
            .count(),
        EXPORT_COUNT
    );
    for position in [0, MIDDLE_EXPORT, EXPORT_COUNT - 1] {
        let name = format!("a{position}");
        let reference = site(facts, "main.ts", (&name, ReferenceKind::Calls));
        assert_target(
            facts,
            reference,
            ("impl.ts", &name, "native-reexport-alias"),
        );
        assert_confidence(reference.confidence, IMPORT_BINDING_CONFIDENCE);
    }
    let names: Vec<_> = (0..DENSE_EXPORT_COUNT)
        .map(|position| format!("a{position}"))
        .collect();
    let dense = format!("export {{ {} }} from './impl';", names.join(", "));
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limit: {error}"));
    let snapshot = cartograph_extract::SourceSnapshot::from_bytes_for_capability_validation(
        "barrel.ts",
        dense.as_bytes(),
        limits,
    )
    .unwrap_or_else(|error| panic!("dense snapshot: {error}"));
    let result = NativeExtractor::new_for_capability_validation(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot));
    assert!(matches!(
        result,
        Err(cartograph_extract::ExtractError::OutputLimit)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reexport_syntax_outside_the_exact_subset_preserves_the_base_target() {
    let built = generation(&[
        ("type/impl.ts", "export function original() {}"),
        ("runtime.ts", "export { original as runtime } from './type/impl';"),
        ("multiline.ts", "export {\n  original as multiline\n} from './type/impl';"),
        ("commented.ts", "export { original as commented /* comment */ } from './type/impl';"),
        ("main.ts", "import { runtime } from './runtime'; import { multiline } from './multiline'; import { commented } from './commented'; export function run() { runtime(); multiline(); commented(); }"),
    ]).await;
    let facts = built.facts();
    assert_target(
        facts,
        site(facts, "main.ts", ("runtime", ReferenceKind::Calls)),
        ("type/impl.ts", "original", "native-reexport-alias"),
    );
    for (name, path) in [("multiline", "multiline.ts"), ("commented", "commented.ts")] {
        assert_target(
            facts,
            site(facts, "main.ts", (name, ReferenceKind::Calls)),
            (path, name, IMPORT_BINDING_PROVENANCE),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reexport_hop_limit_preserves_the_existing_import_target() {
    const ALIAS_CHAIN_LENGTH: usize = 18;
    let mut fixtures = Vec::new();
    for position in 0..ALIAS_CHAIN_LENGTH {
        fixtures.push((
            format!("link{position}.ts"),
            format!("export {{ original }} from './link{}';", position + 1),
        ));
    }
    fixtures.push((
        format!("link{ALIAS_CHAIN_LENGTH}.ts"),
        "export function original() {}".to_owned(),
    ));
    fixtures.push((
        "main.ts".to_owned(),
        "import { original } from './link0'; export function run() { original(); }".to_owned(),
    ));
    let borrowed: Vec<_> = fixtures
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let built = generation(&borrowed).await;
    let facts = built.facts();
    assert_target(
        facts,
        site(facts, "main.ts", ("original", ReferenceKind::Calls)),
        ("link0.ts", "original", IMPORT_BINDING_PROVENANCE),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clojure_direct_recursion_resolves_the_owner_and_parameter_shadowing_abstains() {
    let built = generation(&[
        (
            "src/demo.clj",
            "(ns demo)\n(defn retry ([] (retry 1)) ([n] n))\n(defn shadow [shadow] (shadow 1))\n(defn nested [] (let [nested replacement] (nested 1)))\n",
        ),
        ("src/other.clj", "(ns other) (defn retry [] 1)"),
    ])
    .await;
    let facts = built.facts();
    let reference = site(facts, "src/demo.clj", ("retry", ReferenceKind::Calls));
    assert_target(
        facts,
        reference,
        ("src/demo.clj", "retry", EXACT_SAME_FILE_PROVENANCE),
    );
    let owner = capability_symbol(facts, "src/demo.clj", "retry");
    assert!(facts.edges().iter().any(|edge| edge.kind == EdgeKind::Calls
        && edge.source_symbol_id == owner.symbol_id
        && edge.target_symbol_id == owner.symbol_id));
    assert!(
        site(facts, "src/demo.clj", ("shadow", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
    assert!(
        site(facts, "src/demo.clj", ("nested", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_enum_reads_require_the_nominal_receiver_and_exact_member() {
    let built = generation(&[
        ("Order.java", "package shop; enum Status { NEW, PAID } class Order { Status status; Order() { this.status = Status.NEW; } }"),
        ("Other.java", "package other; enum Status { NEW }"),
        ("Wrong.java", "package bad; enum Status { NEW } class Wrong { int wrong(Object Status) { return Status.NEW; } }"),
    ]).await;
    let facts = built.facts();
    let owner = capability_symbol(facts, "Order.java", "shop::Order::Order");
    let reference =
        CapabilityReferenceQuery::new(facts, owner).named("NEW", ReferenceKind::FieldAccess);
    assert_target(
        facts,
        reference,
        ("Order.java", "shop::Status::NEW", "native-java-enum-member"),
    );
    let wrong = capability_symbol(facts, "Wrong.java", "bad::Wrong::wrong");
    assert!(
        CapabilityReferenceQuery::new(facts, wrong)
            .named("NEW", ReferenceKind::FieldAccess)
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn type_only_reexports_never_supply_runtime_call_targets() {
    let built = generation(&[
        ("impl.ts", "export function original() {}"),
        ("barrel.ts", "export type { original as aliased } from './impl';\nexport { type original as inline } from './impl';"),
        ("main.ts", "import { aliased, inline } from './barrel'; export function run() { aliased(); inline(); }"),
    ]).await;
    let facts = built.facts();
    for name in ["aliased", "inline"] {
        assert!(
            site(facts, "main.ts", (name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_class_member_fallback_abstains_for_decorator_shadowing_and_attribute_writes() {
    let built = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/mutated.py", "class User:\n    @classmethod\n    def anonymous(cls):\n        return cls()\nUser.anonymous = replacement\n"),
        ("pkg/shadowed.py", "classmethod = replacement\nclass User:\n    @classmethod\n    def anonymous(cls):\n        return cls()\n"),
        ("main.py", "from pkg.mutated import User as Mutated\nfrom pkg.shadowed import User as Shadowed\ndef use():\n    Mutated.anonymous()\n    Shadowed.anonymous()\n"),
    ]).await;
    let facts = built.facts();
    for name in ["Mutated.anonymous", "Shadowed.anonymous"] {
        assert!(
            site(facts, "main.py", (name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_wildcard_imports_and_explicit_bindings_withdraw_builtin_decorator_proof() {
    let built = generation(&[
        ("hooks.py", "def replacement():\n    return 2\ndef classmethod(fn):\n    return replacement\ndef staticmethod(fn):\n    return replacement\n"),
        ("model.py", "from hooks import *\nclass User:\n    @classmethod\n    def build(cls):\n        return 1\n"),
        ("named.py", "from hooks import classmethod\nclass User:\n    @classmethod\n    def build(cls):\n        return 1\n"),
        ("static.py", "staticmethod = replacement\nclass User:\n    @staticmethod\n    def build():\n        return 1\n"),
        ("main.py", "from model import User\nfrom named import User as Named\nfrom static import User as Static\ndef use():\n    User.build()\n    Named.build()\n    Static.build()\n"),
    ]).await;
    let facts = built.facts();
    for name in ["User.build", "Named.build", "Static.build"] {
        let reference = site(facts, "main.py", (name, ReferenceKind::Calls));
        assert!(reference.target_symbol_id.is_none(), "{name}");
        assert_eq!(
            reference.resolution_provenance,
            UNRESOLVED_IMPORT_PROVENANCE
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_class_member_calls_abstain_when_decorators_or_class_bindings_replace_them() {
    let built = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/called.py", "def another():\n    return 2\ndef replacement(fn):\n    return another\nclass User:\n    @staticmethod(replacement)\n    def build():\n        return 1\n"),
        ("pkg/decorated.py", "class Replacement:\n    pass\ndef replace(cls):\n    return Replacement\n@replace\nclass User:\n    @staticmethod\n    def build():\n        return 1\n"),
        ("pkg/meta.py", "class Meta(type):\n    pass\nclass User(metaclass=Meta):\n    @staticmethod\n    def build():\n        return 1\n"),
        ("pkg/rebound.py", "class User:\n    @staticmethod\n    def build():\n        return 1\n    build = replacement\n"),
        ("pkg/data.py", "from dataclasses import dataclass\ndataclass = replacement\n@dataclass\nclass User:\n    @staticmethod\n    def build():\n        return 1\n"),
        ("main.py", "from pkg.called import User as Called\nfrom pkg.decorated import User as Decorated\nfrom pkg.meta import User as Meta\nfrom pkg.rebound import User as Rebound\nfrom pkg.data import User as Data\ndef use():\n    Called.build()\n    Decorated.build()\n    Meta.build()\n    Rebound.build()\n    Data.build()\n"),
    ]).await;
    let facts = built.facts();
    for name in [
        "Called.build",
        "Decorated.build",
        "Meta.build",
        "Rebound.build",
        "Data.build",
    ] {
        assert!(
            site(facts, "main.py", (name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_standard_dataclass_keeps_an_explicit_classmethod_target() {
    let built = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/models.py", "from dataclasses import dataclass\n@dataclass\nclass User:\n    @classmethod\n    def anonymous(cls):\n        return cls()\n"),
        ("main.py", "from pkg.models import User\ndef use():\n    User.anonymous()\n"),
    ]).await;
    let facts = built.facts();
    assert_target(
        facts,
        site(facts, "main.py", ("User.anonymous", ReferenceKind::Calls)),
        (
            "pkg/models.py",
            "User::anonymous",
            "native-python-imported-class-member",
        ),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_local_dataclasses_module_blocks_standard_decorator_assumptions() {
    let built = generation(&[
        ("src/dataclasses.py", "def dataclass(cls):\n    return replacement\n"),
        ("pkg/__init__.py", ""),
        ("pkg/models.py", "from dataclasses import dataclass\n@dataclass\nclass User:\n    @classmethod\n    def anonymous(cls):\n        return cls()\n"),
        ("main.py", "from pkg.models import User\ndef use():\n    User.anonymous()\n"),
    ]).await;
    assert!(
        site(
            built.facts(),
            "main.py",
            ("User.anonymous", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_class_member_calls_require_proven_plain_local_bases_and_ascii_bindings() {
    let built = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/plain.py", "class Base:\n    pass\nclass User(Base):\n    @staticmethod\n    def build():\n        return 1\n"),
        ("pkg/meta.py", "class Meta(type):\n    def __getattribute__(cls, name):\n        return super().__getattribute__(name)\nclass Base(metaclass=Meta):\n    pass\nclass User(Base):\n    @staticmethod\n    def build():\n        return 1\n"),
        ("pkg/imported.py", "from external import Base\nclass User(Base):\n    @staticmethod\n    def build():\n        return 1\n"),
        ("pkg/unicode.py", "class User:\n    @staticmethod\n    def K():\n        return 1\n    \u{212a} = replacement\n"),
        ("main.py", "from pkg.plain import User as Plain\nfrom pkg.meta import User as Meta\nfrom pkg.imported import User as Imported\nfrom pkg.unicode import User as Unicode\ndef use():\n    Plain.build()\n    Meta.build()\n    Imported.build()\n    Unicode.K()\n"),
    ]).await;
    let facts = built.facts();
    assert_target(
        facts,
        site(facts, "main.py", ("Plain.build", ReferenceKind::Calls)),
        (
            "pkg/plain.py",
            "User::build",
            "native-python-imported-class-member",
        ),
    );
    for name in ["Meta.build", "Imported.build", "Unicode.K"] {
        assert!(
            site(facts, "main.py", (name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_imported_class_members_follow_aliases_and_abstain_on_rebinding() {
    let built = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/models.py", "class User:\n    @classmethod\n    def anonymous(cls):\n        return cls()\n    @staticmethod\n    def build():\n        return 1\n"),
        ("main.py", "from pkg.models import User as Person\ndef use():\n    Person.anonymous()\n    Person.build()\n"),
        ("blocked.py", "from pkg.models import User as Person\nPerson.anonymous = replacement\ndef use():\n    Person.anonymous()\n"),
        ("missing.py", "from absent.models import User\ndef use():\n    User.anonymous()\n"),
    ]).await;
    let facts = built.facts();
    for (name, target) in [
        ("Person.anonymous", "User::anonymous"),
        ("Person.build", "User::build"),
    ] {
        let reference = site(facts, "main.py", (name, ReferenceKind::Calls));
        assert_target(
            facts,
            reference,
            (
                "pkg/models.py",
                target,
                "native-python-imported-class-member",
            ),
        );
        assert_confidence(reference.confidence, 0.9);
    }
    assert!(
        site(
            facts,
            "blocked.py",
            ("Person.anonymous", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    assert!(
        site(
            facts,
            "missing.py",
            ("User.anonymous", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}
