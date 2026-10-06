use std::fs;

use tempfile::tempdir;

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, IMPORT_BINDING_PROVENANCE, ReferenceInput,
    ReferenceKind, build, build_capability_generation, capability_file_symbol, capability_symbol,
};

const V1_FIXTURES: [(&str, &str); 6] = [
    (
        "app/__init__.py",
        include_str!(
            "../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/__init__.py"
        ),
    ),
    (
        "app/models.py",
        include_str!(
            "../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/models.py"
        ),
    ),
    (
        "app/services.py",
        include_str!(
            "../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/services.py"
        ),
    ),
    (
        "app/utils.py",
        include_str!("../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/utils.py"),
    ),
    (
        "app/sub/__init__.py",
        include_str!(
            "../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/sub/__init__.py"
        ),
    ),
    (
        "app/sub/deep.py",
        include_str!(
            "../../../../cartograph-extract/tests/fixtures/v1_parity/python/app/sub/deep.py"
        ),
    ),
];

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    assert_eq!(forward.references(), reversed.references());
    assert_eq!(forward.edges(), reversed.edges());
    forward
}

fn call<'a>(facts: &'a CanonicalGenerationFacts, path: &str, name: &str) -> &'a ReferenceInput {
    CapabilityReferenceQuery::new(facts, capability_symbol(facts, path, "use"))
        .named(name, ReferenceKind::Calls)
}

fn assert_target(reference: &ReferenceInput, facts: &CanonicalGenerationFacts, path: &str) {
    let target = capability_symbol(facts, path, "run");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert_eq!(reference.confidence, 1.0);
}

#[test]
fn python_absolute_imports_bind_named_and_module_aliases_to_the_exact_file() {
    let facts = generation(&[
        ("app/__init__.py", ""),
        ("app/helpers.py", "def run():\n    pass\n"),
        ("app/sub/deep.py", "def run():\n    pass\n"),
        ("other/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "from app.helpers import run\nimport app.sub.deep as d\nfrom app import helpers\nfrom missing.helpers import run as absent\n\ndef use():\n    run()\n    d.run()\n    helpers.run()\n    absent()\n",
        ),
    ]);
    for name in ["run", "helpers.run"] {
        assert_target(call(&facts, "main.py", name), &facts, "app/helpers.py");
    }
    assert_target(call(&facts, "main.py", "d.run"), &facts, "app/sub/deep.py");
    assert!(call(&facts, "main.py", "absent").target_symbol_id.is_none());
}

#[test]
fn python_module_private_members_require_an_explicit_binding_and_top_level_declaration() {
    let facts = generation(&[
        (
            "pkg/helpers.py",
            "def _run():\n    pass\nclass Hidden:\n    def _nested(self):\n        pass\n",
        ),
        ("other/helpers.py", "def _run():\n    pass\n"),
        (
            "pkg/main.py",
            "from . import helpers as h\n\ndef use():\n    h._run()\n    h._nested()\n    unknown._run()\n",
        ),
    ]);
    let target = capability_symbol(&facts, "pkg/helpers.py", "_run");
    let reference = call(&facts, "pkg/main.py", "h._run");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert_eq!(reference.confidence, 1.0);
    for name in ["h._nested", "unknown._run"] {
        assert!(call(&facts, "pkg/main.py", name).target_symbol_id.is_none());
    }
}

#[test]
fn python_package_initializers_bind_package_members_and_imported_submodules() {
    let facts = generation(&[
        ("pkg/__init__.py", "def helper():\n    pass\n"),
        ("pkg/sub/__init__.py", "def initf():\n    pass\n"),
        ("pkg/sub/deep.py", "def run():\n    pass\n"),
        (
            "pkg/main.py",
            "from . import helper\nfrom .sub import initf, deep\nfrom .missing import initf as absent\n\ndef use():\n    helper()\n    initf()\n    deep.run()\n    absent()\n",
        ),
    ]);
    for (name, path) in [
        ("helper", "pkg/__init__.py"),
        ("initf", "pkg/sub/__init__.py"),
    ] {
        let target = capability_symbol(&facts, path, name);
        let reference = call(&facts, "pkg/main.py", name);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(reference.confidence, 1.0);
    }
    assert_target(
        call(&facts, "pkg/main.py", "deep.run"),
        &facts,
        "pkg/sub/deep.py",
    );
    assert!(
        call(&facts, "pkg/main.py", "absent")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn python_source_root_conventions_have_lower_confidence_and_collisions_abstain() {
    for target in ["src/app/helpers.py", "backend/app/helpers.py"] {
        let package = target.replace("helpers.py", "__init__.py");
        let facts = generation(&[
            (&package, ""),
            (target, "def run():\n    pass\n"),
            (
                "main.py",
                "from app.helpers import run\n\ndef use():\n    run()\n",
            ),
        ]);
        let reference = call(&facts, "main.py", "run");
        let symbol = capability_symbol(&facts, target, "run");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&symbol.symbol_id));
        assert_eq!(reference.resolution_provenance, "native-python-source-root");
        assert_eq!(reference.confidence, 0.9);
    }
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        ("src/app/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "from app.helpers import run\n\ndef use():\n    run()\n",
        ),
    ]);
    assert!(call(&facts, "main.py", "run").target_symbol_id.is_none());
}

#[test]
fn python_package_symbol_and_submodule_collisions_never_guess_a_target() {
    let facts = generation(&[
        ("pkg/__init__.py", "def helper():\n    pass\n"),
        ("pkg/helper.py", "def run():\n    pass\n"),
        ("pkg/sub/__init__.py", "deep = make_object()\n"),
        ("pkg/sub/deep.py", "def run():\n    pass\n"),
        (
            "pkg/main.py",
            "from . import helper\nfrom .sub import deep\n\ndef use():\n    helper()\n    deep.run()\n",
        ),
    ]);
    for name in ["helper", "deep.run"] {
        assert!(call(&facts, "pkg/main.py", name).target_symbol_id.is_none());
    }
}

#[test]
fn python_duplicate_module_members_and_file_package_collisions_abstain() {
    let facts = generation(&[
        (
            "pkg/helpers.py",
            "def run():\n    pass\ndef run():\n    pass\n",
        ),
        ("pkg/conflict.py", "def run():\n    pass\n"),
        ("pkg/conflict/__init__.py", "def run():\n    pass\n"),
        (
            "main.py",
            "import pkg.helpers as h\nimport pkg.conflict as c\n\ndef use():\n    h.run()\n    c.run()\n",
        ),
    ]);
    for name in ["h.run", "c.run"] {
        assert!(call(&facts, "main.py", name).target_symbol_id.is_none());
    }
}

#[test]
fn python_frozen_v1_import_calls_reach_the_recorded_target_files() {
    let facts = generation(&V1_FIXTURES);
    let owner = capability_symbol(&facts, "app/services.py", "UserService::notify");
    for (name, path, target_name) in [
        ("run", "app/utils.py", "run"),
        ("utils.run", "app/utils.py", "run"),
        ("u._private_run", "app/utils.py", "_private_run"),
        ("helper", "app/__init__.py", "helper"),
        ("initf", "app/sub/__init__.py", "initf"),
        ("deep.deep", "app/sub/deep.py", "deep"),
        ("d.deep", "app/sub/deep.py", "deep"),
    ] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        let target = capability_symbol(&facts, path, target_name);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}"
        );
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(reference.confidence, 1.0);
    }
    let create = capability_symbol(&facts, "app/services.py", "UserService::create");
    let target = capability_symbol(&facts, "app/models.py", "make_user");
    for name in ["make_user", "m.make_user"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, create).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }
}

#[test]
fn python_frozen_v1_header_references_keep_their_imported_targets() {
    let facts = generation(&V1_FIXTURES);
    for (owner, name, kind) in [
        ("UserService", "Base", ReferenceKind::Extends),
        ("UserService::__init__", "Repo", ReferenceKind::TypeOf),
        ("UserService::create", "User", ReferenceKind::Returns),
    ] {
        let owner = capability_symbol(&facts, "app/services.py", owner);
        let reference = CapabilityReferenceQuery::new(&facts, owner).named(name, kind);
        let target = capability_symbol(&facts, "app/models.py", name);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}"
        );
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(reference.confidence, 1.0);
    }
}

#[test]
fn python_header_types_observe_the_enclosing_class_shadow() {
    let facts = generation(&[
        ("app/models.py", "class Imported:\n    pass\n"),
        (
            "main.py",
            "from app.models import Imported as T\n\nclass C:\n    T = make_object()\n    def use(self) -> T:\n        pass\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "main.py", "C::use");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("T", ReferenceKind::Returns);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn python_external_imports_and_ambiguous_unaliased_dotted_bindings_abstain() {
    let facts = generation(&[
        ("json.py", "def run():\n    pass\n"),
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "import json as j\nimport app.helpers\nfrom app.helpers import run as aliased\n\ndef use():\n    j.run()\n    app.run()\n    app.helpers.run()\n    aliased()\n",
        ),
    ]);
    for name in ["j.run", "app.run", "app.helpers.run"] {
        assert!(call(&facts, "main.py", name).target_symbol_id.is_none());
    }
    assert_target(call(&facts, "main.py", "aliased"), &facts, "app/helpers.py");
}

#[test]
fn python_import_references_name_the_module_file_or_package_member_and_preserve_ambiguity() {
    let facts = generation(&[
        ("app/__init__.py", "def helper():\n    pass\n"),
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "app/main.py",
            "import app.helpers as h\nfrom app.helpers import run\nfrom . import helper\nfrom . import helpers as a\n",
        ),
    ]);
    let owner = capability_file_symbol(&facts, "app/main.py");
    let helpers = capability_file_symbol(&facts, "app/helpers.py");
    let imports = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference.reference_name == "app.helpers"
                && reference.reference_kind == ReferenceKind::Imports.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    for imported in imports {
        assert_eq!(imported.target_symbol_id.as_ref(), Some(&helpers.symbol_id));
        assert_eq!(imported.resolution_provenance, "native-module-import");
    }
    let helper = capability_symbol(&facts, "app/__init__.py", "helper");
    let imported =
        CapabilityReferenceQuery::new(&facts, owner).named("./helper", ReferenceKind::Imports);
    assert_eq!(imported.target_symbol_id.as_ref(), Some(&helper.symbol_id));
    assert_eq!(imported.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    let imported =
        CapabilityReferenceQuery::new(&facts, owner).named("./helpers", ReferenceKind::Imports);
    assert_eq!(imported.target_symbol_id.as_ref(), Some(&helpers.symbol_id));
    assert_eq!(imported.resolution_provenance, "native-module-import");
    let collision = generation(&[
        ("app/__init__.py", "helper = make_object()\n"),
        ("app/helper.py", "def run():\n    pass\n"),
        ("app/main.py", "from . import helper\n"),
    ]);
    let owner = capability_file_symbol(&collision, "app/main.py");
    let imported =
        CapabilityReferenceQuery::new(&collision, owner).named("./helper", ReferenceKind::Imports);
    assert!(imported.target_symbol_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_import_resolution_is_identical_with_every_supported_worker_count() {
    let directory = tempdir().unwrap_or_else(|error| panic!("Python fixture: {error}"));
    let source_root_fixtures = [
        ("src/tools/__init__.py", ""),
        ("src/tools/helpers.py", "def _run():\n    pass\n"),
        (
            "main.py",
            "import tools.helpers as h\n\ndef use():\n    h._run()\n",
        ),
    ];
    for (path, source) in V1_FIXTURES.iter().chain(&source_root_fixtures) {
        let target = directory.path().join(path);
        assert!(fs::create_dir_all(target.parent().unwrap_or(directory.path())).is_ok());
        assert!(fs::write(target, source).is_ok());
    }
    let serial = build(directory.path(), 1).await;
    let target = capability_symbol(serial.facts(), "src/tools/helpers.py", "_run");
    let reference = call(serial.facts(), "main.py", "h._run");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, "native-python-source-root");
    assert_eq!(reference.confidence, 0.9);
    for workers in [2, 4, 8, 16] {
        let parallel = build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers}"
        );
        assert_eq!(serial.facts().references(), parallel.facts().references());
        assert_eq!(serial.facts().edges(), parallel.facts().edges());
    }
}

#[test]
fn python_file_wide_bindings_and_nested_imports_block_import_fallback() {
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "import app.helpers as h\nimport app.helpers as module\nfrom app.helpers import run\n\ndef parameter(h):\n    h.run()\ndef named_parameter(run):\n    run()\ndef assigned():\n    h = make_object()\n    h.run()\ndef load():\n    import app.helpers as local\n    local.run()\ndef sibling():\n    local.run()\ndef control():\n    module.run()\n",
        ),
    ]);
    for (owner, name) in [
        ("parameter", "h.run"),
        ("named_parameter", "run"),
        ("assigned", "h.run"),
        ("load", "local.run"),
        ("sibling", "local.run"),
    ] {
        let owner = capability_symbol(&facts, "main.py", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{name}: {reference:?}"
        );
    }
    let owner = capability_symbol(&facts, "main.py", "control");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("module.run", ReferenceKind::Calls);
    assert_target(reference, &facts, "app/helpers.py");
}

#[test]
fn python_guarded_imports_and_unknown_binding_forms_abstain() {
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "import app.helpers as h\n\ndef guarded():\n    if enabled:\n        import app.helpers as local\n    local.run()\ndef matched(value):\n    match value:\n        case h:\n            h.run()\n",
        ),
        (
            "wildcard.py",
            "import app.helpers as h\nfrom external import *\n\ndef wildcard():\n    h.run()\n",
        ),
    ]);
    for (path, owner, name) in [
        ("main.py", "guarded", "local.run"),
        ("main.py", "matched", "h.run"),
        ("wildcard.py", "wildcard", "h.run"),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{name}: {reference:?}"
        );
    }
}

#[test]
fn python_comprehensions_skip_class_bindings_and_keep_their_own_shadows() {
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "class_local.py",
            "class C:\n    import app.helpers as h\n    values = [h.run() for n in values]\n",
        ),
        (
            "class_shadow.py",
            "import app.helpers as h\n\nclass C:\n    values = [h.run() for h in values]\n",
        ),
        (
            "class_global.py",
            "import app.helpers as h\n\nclass C:\n    values = [h.run() for n in values]\n",
        ),
    ]);
    for path in ["class_local.py", "class_shadow.py"] {
        let owner = capability_symbol(&facts, path, "C");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("h.run", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{path}: {reference:?}"
        );
    }
    let owner = capability_symbol(&facts, "class_global.py", "C");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("h.run", ReferenceKind::Calls);
    assert_target(reference, &facts, "app/helpers.py");
}

#[test]
fn python_import_visibility_abstains_in_unsupported_evaluation_and_binding_scopes() {
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "iterable.py",
            "import app.helpers as h\n\nclass C:\n    h = make_object()\n    values = [n for n in h.run()]\n",
        ),
        (
            "nested.py",
            "class C:\n    import app.helpers as h\n    class D:\n        value = h.run()\n",
        ),
        (
            "except.py",
            "import app.helpers as h\n\ndef use():\n    try:\n        pass\n    except Exception as h:\n        h.run()\n",
        ),
        (
            "alias.py",
            "import app.helpers as h\ntype h = object\n\ndef use():\n    h.run()\n",
        ),
        (
            "parameter.py",
            "import app.helpers as h\n\nclass C[h]:\n    def use(self):\n        h.run()\n",
        ),
    ]);
    for path in [
        "iterable.py",
        "nested.py",
        "except.py",
        "alias.py",
        "parameter.py",
    ] {
        let file = capability_file_symbol(&facts, path);
        let references = facts
            .references()
            .iter()
            .filter(|reference| {
                reference.file_id == file.file_id
                    && reference.reference_name == "h.run"
                    && reference.reference_kind == ReferenceKind::Calls.as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 1, "{path}: {references:?}");
        let reference = references[0];
        assert!(
            reference.target_symbol_id.is_none(),
            "{path}: {reference:?}"
        );
    }
}

#[test]
fn python_comprehension_and_header_writes_shadow_the_enclosing_import_binding() {
    let facts = generation(&[
        ("app/helpers.py", "def run():\n    pass\n"),
        (
            "comprehension.py",
            "import app.helpers as h\n\ndef use():\n    values = [(h := make_object()) for n in values]\n    h.run()\n",
        ),
        (
            "header.py",
            "import app.helpers as h\n\ndef other(value=(h := make_object())):\n    pass\n\ndef use():\n    h.run()\n",
        ),
        (
            "control.py",
            "import app.helpers as h\n\ndef use():\n    values = [n for n in values]\n    h.run()\n",
        ),
    ]);
    for path in ["comprehension.py", "header.py"] {
        let reference = call(&facts, path, "h.run");
        assert!(
            reference.target_symbol_id.is_none(),
            "{path}: {reference:?}"
        );
    }
    assert_target(
        call(&facts, "control.py", "h.run"),
        &facts,
        "app/helpers.py",
    );
}

#[test]
fn python_verifier_attribute_write_blocks_the_imported_member() {
    let facts = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "import pkg.helpers as h\ndef replacement():\n    return 2\nh.run = replacement\ndef use():\n    return h.run()\n",
        ),
    ]);
    let reference = call(&facts, "main.py", "h.run");
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn python_verifier_regular_package_descendants_cannot_cross_source_roots() {
    let facts = generation(&[
        ("pkg/__init__.py", ""),
        ("src/pkg/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "from pkg import helpers\nimport pkg.helpers as h\ndef use():\n    helpers.run()\n    h.run()\n",
        ),
    ]);
    for name in ["helpers.run", "h.run"] {
        let reference = call(&facts, "main.py", name);
        assert!(
            reference.target_symbol_id.is_none(),
            "{name}: {reference:?}"
        );
    }
}

#[test]
fn python_verifier_base_correct_local_and_import_targets_are_preserved() {
    let facts = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/helpers.py", "def run():\n    pass\n"),
        (
            "main.py",
            "def run():\n    return 1\ndef load():\n    from pkg.helpers import run\ndef use():\n    run()\n",
        ),
        (
            "pkg/global.py",
            "from .helpers import run\ndef use():\n    global counter\n    run()\n",
        ),
        (
            "pkg/late.py",
            "def use():\n    run()\nfrom .helpers import run\nuse()\n",
        ),
    ]);
    let local = call(&facts, "main.py", "run");
    let target = capability_symbol(&facts, "main.py", "run");
    assert_eq!(local.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(local.resolution_provenance, "native-exact-same-file");
    assert_eq!(local.confidence, 1.0);
    for path in ["pkg/global.py", "pkg/late.py"] {
        assert_target(call(&facts, path, "run"), &facts, "pkg/helpers.py");
    }
}

#[test]
fn python_verifier_leading_comments_and_repeated_calls_keep_exact_targets() {
    let count = 128;
    let source = format!(
        "{}import pkg.helpers as h\ndef use():\n{}",
        "# leading comment\n".repeat(count),
        "    h.run()\n".repeat(count),
    );
    let facts = generation(&[
        ("pkg/__init__.py", ""),
        ("pkg/helpers.py", "def run():\n    pass\n"),
        ("main.py", &source),
    ]);
    let owner = capability_symbol(&facts, "main.py", "use");
    let references = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference.reference_name == "h.run"
                && reference.reference_kind == ReferenceKind::Calls.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), count);
    for reference in references {
        assert_target(reference, &facts, "pkg/helpers.py");
    }
}

#[test]
fn python_import_fallback_abstains_for_every_supported_binding_target() {
    for binding in [
        "h = make_object()\n",
        "h += make_object()\n",
        "h: object\n",
        "h, other = values\n",
        "def other(h):\n    pass\n",
        "def other(h: object = None):\n    pass\n",
        "def h():\n    pass\n",
        "class h:\n    pass\n",
        "for h in values:\n    pass\n",
        "with context as h:\n    pass\n",
        "try:\n    pass\nexcept Exception as h:\n    pass\n",
        "values = [h for h in values]\n",
        "values = (h := make_object())\n",
        "def other():\n    global h\n",
        "def outer(h):\n    def inner():\n        nonlocal h\n",
        "del h\n",
        "h.run = replacement\n",
        "h.other = replacement\n",
    ] {
        let source = format!("import pkg.helpers as h\n{binding}def use():\n    h.run()\n");
        let facts = generation(&[
            ("pkg/__init__.py", ""),
            ("pkg/helpers.py", "def run():\n    pass\n"),
            ("main.py", &source),
        ]);
        let reference = call(&facts, "main.py", "h.run");
        assert!(
            reference.target_symbol_id.is_none(),
            "{binding}: {reference:?}"
        );
    }
}
