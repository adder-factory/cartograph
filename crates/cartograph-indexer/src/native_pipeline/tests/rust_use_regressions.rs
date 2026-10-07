use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceKind, build_capability_generation,
    capability_symbol,
};

const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

#[test]
fn rust_unrelated_patterns_keep_bare_child_and_alias_calls_in_impl_methods() {
    for pattern in [
        "let (value,) = (1,);",
        "let Some(value) = Some(1) else { return; };",
        "let Record { value, .. } = record;",
        "let Record { helper: renamed, .. } = record;",
        "let [first, second] = [1, 2];",
        "let ref value = 1;",
        "let mut value = 1;",
        "let bound @ Some(_) = Some(1) else { return; };",
        "let _ = (|row| row)(1);",
        "let _ = (|(value,)| value)((1,));",
        "match (1, Some(2)) { (_, Some(value)) => (), _ => () }",
    ] {
        let source = format!(
            "mod api; use api::helper; use crate::database::validate_bounded_limit as validate_limit; struct Consumer; struct Record {{ value: i32, helper: fn() }} impl Consumer {{ pub fn run() {{ {pattern} helper(); validate_limit(); }} }} mod tests {{ use super::*; }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod consumer; mod database;"),
                ("src/consumer.rs", &source),
                ("src/consumer/api.rs", "pub(super) fn helper() {}"),
                (
                    "src/database.rs",
                    "pub(crate) fn validate_bounded_limit() {}",
                ),
            ],
            false,
        );
        for (name, file, target) in [
            ("helper", "src/consumer/api.rs", "helper"),
            (
                "validate_limit",
                "src/database.rs",
                "validate_bounded_limit",
            ),
        ] {
            assert_use(
                &facts,
                (
                    "src/consumer.rs",
                    "Consumer::run",
                    name,
                    ReferenceKind::Calls,
                ),
                (file, target),
            );
        }
    }
}

#[test]
fn rust_pattern_names_and_unknown_patterns_still_fence_matching_bare_calls() {
    for body in [
        "let (helper,) = (|| {},); helper();",
        "let Some(helper) = Some(|| {}) else { return; }; helper();",
        "let Record { helper } = record; helper();",
        "let Record { other: helper } = record; helper();",
        "let [helper] = [|| {}]; helper();",
        "let ref helper = value; helper();",
        "let mut helper = || {}; helper();",
        "let helper @ Some(_) = value else { return; }; helper();",
        "let binding!() = value; helper();",
        "let _ = (|helper| helper())(value);",
        "let _ = (|(helper,)| helper())(value);",
    ] {
        let source = format!("use crate::api::helper; pub fn run() {{ {body} }}");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod consumer; mod api; mod decoy;"),
                ("src/consumer.rs", &source),
                ("src/api.rs", "pub fn helper() {}"),
                ("src/decoy.rs", "pub fn helper() {}"),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            ("src/consumer.rs", "run", "helper", ReferenceKind::Calls),
        );
    }
}

#[test]
fn rust_equivalent_identifier_spellings_keep_value_shadow_fences() {
    for (alias, pattern, call) in [
        ("helper", "r#helper", "helper"),
        ("r#helper", "helper", "r#helper"),
        ("caf\u{e9}", "cafe\u{301}", "caf\u{e9}"),
        ("\u{212a}", "K", "\u{212a}"),
    ] {
        let source = format!(
            "use crate::api::helper as {alias}; pub fn run() {{ let Some({pattern}) = Some(|| {{}}) else {{ return; }}; {call}(); }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod consumer; mod api; mod decoy;"),
                ("src/consumer.rs", &source),
                ("src/api.rs", "pub fn helper() {}"),
                ("src/decoy.rs", "pub fn helper() {}"),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            ("src/consumer.rs", "run", call, ReferenceKind::Calls),
        );
    }
}

#[test]
fn rust_saturated_pattern_scans_keep_unknown_name_fences() {
    let bindings = std::iter::repeat_n("value", 4096)
        .chain(["helper"])
        .collect::<Vec<_>>()
        .join(", ");
    let source =
        format!("use crate::api::helper; pub fn run() {{ let ({bindings}) = values; helper(); }}");
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod consumer; mod api; mod decoy;"),
            ("src/consumer.rs", &source),
            ("src/api.rs", "pub fn helper() {}"),
            ("src/decoy.rs", "pub fn helper() {}"),
        ],
        false,
    );
    assert_unresolved(
        &facts,
        ("src/consumer.rs", "run", "helper", ReferenceKind::Calls),
    );
}

#[test]
fn rust_inline_file_children_obey_the_declaring_modules_privacy_subtree() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub mod outer { mod child; pub fn inside() { self::child::helper(); } pub mod descendant { pub fn inside() { super::child::helper(); } } } pub fn outside() { crate::outer::child::helper(); }",
            ),
            ("src/outer/child.rs", "pub fn helper() {}"),
        ],
        false,
    );
    for (owner, name) in [
        ("outer::inside", "self::child::helper"),
        ("outer::descendant::inside", "super::child::helper"),
    ] {
        let owner = capability_symbol(&facts, "src/lib.rs", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        let target = capability_symbol(&facts, "src/outer/child.rs", "helper");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(
            reference.resolution_provenance,
            "native-rust-qualified-path"
        );
        assert_eq!(reference.confidence, 1.0);
    }
    assert_unresolved(
        &facts,
        (
            "src/lib.rs",
            "outside",
            "crate::outer::child::helper",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_attributed_inline_parents_withhold_conventional_child_proofs() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "#[path = \"redirect\"] pub mod outer { mod child; pub fn run() { self::child::helper(); } }",
            ),
            ("src/outer/child.rs", "pub fn helper() {}"),
            ("src/redirect/child.rs", "pub fn helper() {}"),
        ],
        false,
    );
    assert_unresolved(
        &facts,
        (
            "src/lib.rs",
            "outer::run",
            "self::child::helper",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_commented_child_path_attributes_withhold_conventional_module_edges() {
    for attribute in [
        "#[path = \"../redirect/child.rs\"]",
        "#[cfg_attr(feature = \"redirect\", path = \"../redirect/child.rs\")]",
    ] {
        let source = format!(
            "pub mod outer {{ {attribute} /* comment */ mod child; pub fn run() {{ self::child::helper(); }} }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/outer/child.rs", "pub fn helper() {}"),
                ("src/redirect/child.rs", "pub fn helper() {}"),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            (
                "src/lib.rs",
                "outer::run",
                "self::child::helper",
                ReferenceKind::Calls,
            ),
        );
    }
}

#[test]
fn rust_declared_super_globs_keep_standard_assertions_and_fence_unknown_macros() {
    for assertion in ["assert!(true);", "make_items!();"] {
        let consumer =
            format!("use super::*; pub fn run() {{ let _ = api::PROVENANCE; {assertion} }}");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod pipeline;"),
                (
                    "src/pipeline.rs",
                    "mod api; #[cfg(test)] mod tests { mod consumer; use super::*; }",
                ),
                (
                    "src/pipeline/api.rs",
                    "pub(super) const PROVENANCE: &str = \"exact\";",
                ),
                ("src/pipeline/tests/consumer.rs", &consumer),
            ],
            false,
        );
        let source = (
            "src/pipeline/tests/consumer.rs",
            "run",
            "api::PROVENANCE",
            ReferenceKind::References,
        );
        if assertion.starts_with("assert!") {
            assert_use(&facts, source, ("src/pipeline/api.rs", "PROVENANCE"));
        } else {
            assert_unresolved(&facts, source);
        }
    }
}

#[test]
fn rust_parent_macro_overrides_withdraw_standard_super_glob_proofs() {
    for extra in [
        "macro_rules! assert { ($($tt:tt)*) => { mod api {} }; }",
        "use external::assert;",
        "use external::*;",
        "#[macro_use] mod external;",
    ] {
        let parent =
            format!("mod api; {extra} #[cfg(test)] mod tests {{ mod consumer; use super::*; }}");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod pipeline;"),
                ("src/pipeline.rs", &parent),
                (
                    "src/pipeline/api.rs",
                    "pub(super) const PROVENANCE: &str = \"exact\";",
                ),
                (
                    "src/pipeline/tests/consumer.rs",
                    "use super::*; pub fn run() { let _ = api::PROVENANCE; assert!(true); }",
                ),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            (
                "src/pipeline/tests/consumer.rs",
                "run",
                "api::PROVENANCE",
                ReferenceKind::References,
            ),
        );
    }
}

#[test]
fn rust_textual_macro_ancestors_fence_super_globs_without_intermediate_globs() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "macro_rules! assert { ($e:expr) => { mod api { pub const PROVENANCE: &str = \"local\"; } }; } mod parent;",
            ),
            ("src/parent.rs", "mod api; mod consumer;"),
            (
                "src/parent/api.rs",
                "pub(super) const PROVENANCE: &str = \"imported\";",
            ),
            (
                "src/parent/consumer.rs",
                "use super::*; pub fn run() -> &'static str { assert!(true); api::PROVENANCE }",
            ),
        ],
        false,
    );
    assert_unresolved(
        &facts,
        (
            "src/parent/consumer.rs",
            "run",
            "api::PROVENANCE",
            ReferenceKind::References,
        ),
    );
}

#[test]
fn rust_cargo_standard_namespace_aliases_keep_item_macro_fences() {
    for alias in ["std", "core"] {
        let manifest = format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\n{alias} = {{ package = \"custom\", path = \"../custom\" }}\n"
        );
        let source = format!(
            "mod api; use crate::api::helper; {alias}::compile_error!(); pub fn run() {{ helper(); }}"
        );
        let facts = build_capability_generation(
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"crates/app\", \"crates/custom\"]\n",
                ),
                ("crates/app/Cargo.toml", &manifest),
                ("crates/app/src/lib.rs", &source),
                ("crates/app/src/api.rs", "pub fn helper() {}"),
                ("crates/app/src/unused.rs", "pub fn helper() {}"),
                (
                    "crates/custom/Cargo.toml",
                    "[package]\nname = \"custom\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "crates/custom/src/lib.rs",
                    "#[macro_export] macro_rules! compile_error { () => { fn helper() {} }; }",
                ),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            (
                "crates/app/src/lib.rs",
                "run",
                "helper",
                ReferenceKind::Calls,
            ),
        );
    }
}

#[test]
fn rust_inline_test_modules_keep_two_hop_super_glob_constant_paths() {
    for extra in [
        "",
        "use unknown::*;",
        "use external::api;",
        "pub use external::api;",
        "make_items!();",
    ] {
        let parent =
            format!("mod api; #[cfg(test)] mod tests {{ mod consumer; use super::*; {extra} }}");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", "mod pipeline;"),
                ("src/pipeline.rs", &parent),
                (
                    "src/pipeline/api.rs",
                    "pub(super) const PROVENANCE: &str = \"exact\";",
                ),
                (
                    "src/pipeline/tests/consumer.rs",
                    "use super::*; pub fn run() -> &'static str { api::PROVENANCE }",
                ),
            ],
            false,
        );
        if extra.is_empty() {
            assert_use(
                &facts,
                (
                    "src/pipeline/tests/consumer.rs",
                    "run",
                    "api::PROVENANCE",
                    ReferenceKind::References,
                ),
                ("src/pipeline/api.rs", "PROVENANCE"),
            );
        } else {
            assert_unresolved(
                &facts,
                (
                    "src/pipeline/tests/consumer.rs",
                    "run",
                    "api::PROVENANCE",
                    ReferenceKind::References,
                ),
            );
        }
    }
}

#[test]
fn rust_uniform_heads_abstain_for_cfg_alternative_modules_and_nominals() {
    for source in [
        "#[cfg(a)] pub mod tools { pub fn helper() {} } #[cfg(b)] mod tools { pub fn helper() {} } use tools::helper; pub fn run() { helper(); }",
        "#[cfg(a)] pub mod tools { pub fn helper() {} } #[cfg(b)] pub struct tools; #[cfg(b)] impl tools { pub fn helper() {} } pub fn run() { tools::helper(); }",
    ] {
        let facts =
            build_capability_generation(&[("Cargo.toml", MANIFEST), ("src/lib.rs", source)], false);
        let name = if source.contains("use tools") {
            "helper"
        } else {
            "tools::helper"
        };
        assert_unresolved(&facts, ("src/lib.rs", "run", name, ReferenceKind::Calls));
    }
}

#[test]
fn rust_unicode_child_module_uses_keep_exact_import_targets() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "mod café; use café::helper; pub fn run() { helper(); }",
            ),
            ("src/café.rs", "pub fn helper() {}"),
        ],
        false,
    );
    assert_use(
        &facts,
        ("src/lib.rs", "run", "helper", ReferenceKind::Calls),
        ("src/café.rs", "helper"),
    );
}

#[test]
fn rust_lifetime_function_type_annotations_do_not_shadow_module_uses() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod consumer; mod walk;"),
            (
                "src/walk.rs",
                "pub(crate) struct WalkInput<'a>(&'a ()); pub(crate) fn extract() {}",
            ),
            (
                "src/consumer.rs",
                "use crate::walk; pub fn run<'a>(_: walk::WalkInput<'a>) { apply(|| walk::extract()); } pub fn shadowed<walk>() { walk::extract(); } fn apply(f: impl Fn()) { f(); }",
            ),
        ],
        false,
    );
    assert_use(
        &facts,
        (
            "src/consumer.rs",
            "run",
            "walk::extract",
            ReferenceKind::Calls,
        ),
        ("src/walk.rs", "extract"),
    );
    assert_unresolved(
        &facts,
        (
            "src/consumer.rs",
            "shadowed",
            "walk::extract",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_builtin_compile_errors_do_not_withdraw_child_constant_uses() {
    for invocation in [
        "compile_error!",
        "std::compile_error!",
        "core::compile_error!",
    ] {
        let source = format!(
            "#[cfg(not(target_pointer_width = \"64\"))] {invocation}(\"requires 64 bits\"); mod helpers; use helpers::LIMIT; pub fn run() -> u32 {{ LIMIT }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/main.rs", &source),
                ("src/helpers.rs", "pub(super) const LIMIT: u32 = 7;"),
            ],
            false,
        );
        assert_use(
            &facts,
            ("src/main.rs", "run", "LIMIT", ReferenceKind::References),
            ("src/helpers.rs", "LIMIT"),
        );
    }
}

#[test]
fn rust_shadowed_compile_error_macros_still_fence_the_module() {
    for source in [
        "macro_rules! compile_error { () => { const LIMIT: u32 = 9; } } compile_error!(); mod helpers; use helpers::LIMIT; pub fn run() -> u32 { LIMIT }",
        "use another::make_items as compile_error; compile_error!(); mod helpers; use helpers::LIMIT; pub fn run() -> u32 { LIMIT }",
        "use external as std; std::compile_error!(); mod helpers; use helpers::LIMIT; pub fn run() -> u32 { LIMIT }",
        "use external as core; core::compile_error!(); mod helpers; use helpers::LIMIT; pub fn run() -> u32 { LIMIT }",
    ] {
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/main.rs", source),
                ("src/helpers.rs", "pub(super) const LIMIT: u32 = 7;"),
            ],
            false,
        );
        assert_unresolved(
            &facts,
            ("src/main.rs", "run", "LIMIT", ReferenceKind::References),
        );
    }
}

#[test]
fn rust_lifetime_only_root_impls_keep_uses_without_admitting_type_generics() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod references; mod consumer;"),
            ("src/references.rs", "pub(super) fn resolve() {}"),
            (
                "src/consumer.rs",
                "use super::references; struct Scan<'a>(&'a ()); impl<'a> Scan<'a> { pub fn run() { references::resolve(); } } struct Generic<T>(T); impl<references> Generic<references> { pub fn run() { references::resolve(); } }",
            ),
        ],
        false,
    );
    assert_use(
        &facts,
        (
            "src/consumer.rs",
            "Scan::run",
            "references::resolve",
            ReferenceKind::Calls,
        ),
        ("src/references.rs", "resolve"),
    );
    assert_unresolved(
        &facts,
        (
            "src/consumer.rs",
            "Generic::run",
            "references::resolve",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_root_type_uses_still_reject_private_targets_and_nested_bindings() {
    for source in [
        "mod inner { struct Slot {} } use self::inner::Slot; pub struct Use { value: Slot }",
        "mod inner { pub struct Slot {} } mod other { use self::inner::Slot; } pub struct Use { value: Slot }",
    ] {
        let facts = build_capability_generation(&[("lib.rs", source)], false);
        let owner = capability_symbol(&facts, "lib.rs", "Use");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::TypeOf);
        assert!(
            reference.target_symbol_id.is_none(),
            "{source}: {reference:?}"
        );
    }
}

#[test]
fn rust_grouped_root_uses_survive_nested_test_imports_in_impl_methods() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "mod consumer; mod walk; mod custom; mod tags; mod framework;",
            ),
            (
                "src/consumer.rs",
                "use crate::{walk, custom, tags, framework}; struct Extractor; impl Extractor { pub fn run() { walk::WalkInput::new(); walk::extract(); custom::extract(); custom::scans_snapshot(); tags::extract(); framework::enrich(); } } pub fn shadowed<walk>() { walk::extract(); } #[cfg(test)] mod tests { use super::*; use crate::walk; }",
            ),
            (
                "src/walk.rs",
                "pub(crate) struct WalkInput; impl WalkInput { pub(crate) fn new() {} } pub(crate) fn extract() {}",
            ),
            (
                "src/custom.rs",
                "pub(crate) fn extract() {} pub(crate) fn scans_snapshot() {}",
            ),
            ("src/tags.rs", "pub(crate) fn extract() {}"),
            ("src/framework.rs", "pub(crate) fn enrich() {}"),
        ],
        false,
    );
    for (module, member) in [
        ("walk", "WalkInput::new"),
        ("walk", "extract"),
        ("custom", "extract"),
        ("custom", "scans_snapshot"),
        ("tags", "extract"),
        ("framework", "enrich"),
    ] {
        assert_use(
            &facts,
            (
                "src/consumer.rs",
                "Extractor::run",
                &format!("{module}::{member}"),
                ReferenceKind::Calls,
            ),
            (&format!("src/{module}.rs"), member),
        );
    }
    assert_unresolved(
        &facts,
        (
            "src/consumer.rs",
            "shadowed",
            "walk::extract",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_child_module_uses_resolve_functions_and_associated_items() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod supervisor;"),
            (
                "src/supervisor.rs",
                "mod lease_keeper; use lease_keeper::{run_bounded_heartbeat, RenewalWindow}; pub fn run() { run_bounded_heartbeat(); RenewalWindow::reaping(); } pub fn shadowed<RenewalWindow>() { RenewalWindow::reaping(); }",
            ),
            (
                "src/supervisor/lease_keeper.rs",
                "pub(super) fn run_bounded_heartbeat() {} pub(super) struct RenewalWindow; impl RenewalWindow { pub(super) fn reaping() {} }",
            ),
        ],
        false,
    );
    for name in ["run_bounded_heartbeat", "RenewalWindow::reaping"] {
        assert_use(
            &facts,
            ("src/supervisor.rs", "run", name, ReferenceKind::Calls),
            ("src/supervisor/lease_keeper.rs", name),
        );
    }
    assert_unresolved(
        &facts,
        (
            "src/supervisor.rs",
            "shadowed",
            "RenewalWindow::reaping",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_child_module_constructors_resolve_inside_plain_root_impls() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod managed;"),
            (
                "src/managed/mod.rs",
                "mod docker; mod credentials; use docker::DockerCli; use credentials::CredentialStore; struct Managed; impl Managed { pub fn new() { DockerCli::new(); CredentialStore::new(); } pub fn shadowed<DockerCli>() { DockerCli::new(); } }",
            ),
            (
                "src/managed/docker.rs",
                "pub(super) struct DockerCli; impl DockerCli { pub(super) fn new() {} }",
            ),
            (
                "src/managed/credentials.rs",
                "pub(super) struct CredentialStore; impl CredentialStore { pub(super) fn new() {} }",
            ),
        ],
        false,
    );
    for (module, name) in [("docker", "DockerCli"), ("credentials", "CredentialStore")] {
        assert_use(
            &facts,
            (
                "src/managed/mod.rs",
                "Managed::new",
                &format!("{name}::new"),
                ReferenceKind::Calls,
            ),
            (&format!("src/managed/{module}.rs"), &format!("{name}::new")),
        );
    }
    assert_unresolved(
        &facts,
        (
            "src/managed/mod.rs",
            "Managed::shadowed",
            "DockerCli::new",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_child_mapping_imports_do_not_bind_unrelated_nested_uses() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod v1_import; mod other;"),
            ("src/other.rs", "pub fn hidden() {}"),
            (
                "src/v1_import.rs",
                "mod mapping; use mapping::map_source_facts; pub fn run() { map_source_facts(); } mod nested { use crate::v1_import::mapping::hidden; } pub fn missing() { hidden(); }",
            ),
            (
                "src/v1_import/mapping.rs",
                "pub(super) fn map_source_facts() {} pub fn hidden() {}",
            ),
        ],
        false,
    );
    assert_use(
        &facts,
        (
            "src/v1_import.rs",
            "run",
            "map_source_facts",
            ReferenceKind::Calls,
        ),
        ("src/v1_import/mapping.rs", "map_source_facts"),
    );
    assert_unresolved(
        &facts,
        (
            "src/v1_import.rs",
            "missing",
            "hidden",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_library_root_imports_resolve_crate_visible_failure_helpers() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "use index_failure::{recover_abandoned_staging, supervisor_index_failure}; mod index_failure; pub fn run() { recover_abandoned_staging(); supervisor_index_failure(); private(); }",
            ),
            (
                "src/index_failure.rs",
                "pub(crate) fn recover_abandoned_staging() {} pub(crate) fn supervisor_index_failure() {} fn private() {}",
            ),
        ],
        false,
    );
    for name in ["recover_abandoned_staging", "supervisor_index_failure"] {
        assert_use(
            &facts,
            ("src/lib.rs", "run", name, ReferenceKind::Calls),
            ("src/index_failure.rs", name),
        );
    }
    assert_unresolved(
        &facts,
        ("src/lib.rs", "run", "private", ReferenceKind::Calls),
    );
}

#[test]
fn rust_binary_root_imports_resolve_child_constants_and_nominal_items() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub fn library_only() {}"),
            (
                "src/main.rs",
                "mod graph_export; use graph_export::{DEFAULT_NODE_LIMIT, GraphExportFormat, GraphExportRequest, PRIVATE_LIMIT}; fn main() { let _ = DEFAULT_NODE_LIMIT; GraphExportRequest::new(); } fn select(value: GraphExportFormat) {} fn missing() { PRIVATE_LIMIT; } #[cfg(test)] mod tests { use super::*; }",
            ),
            (
                "src/graph_export.rs",
                "pub(super) const DEFAULT_NODE_LIMIT: u16 = 1000; pub(super) enum GraphExportFormat { Json } pub(super) struct GraphExportRequest; impl GraphExportRequest { pub(super) fn new() {} } const PRIVATE_LIMIT: u16 = 1;",
            ),
        ],
        false,
    );
    for (owner, name, kind) in [
        ("main", "DEFAULT_NODE_LIMIT", ReferenceKind::References),
        ("main", "GraphExportRequest::new", ReferenceKind::Calls),
        ("select", "GraphExportFormat", ReferenceKind::TypeOf),
    ] {
        assert_use(
            &facts,
            ("src/main.rs", owner, name, kind),
            ("src/graph_export.rs", name),
        );
    }
    assert_unresolved(
        &facts,
        (
            "src/main.rs",
            "missing",
            "PRIVATE_LIMIT",
            ReferenceKind::References,
        ),
    );
}

#[test]
fn rust_grouped_alias_uses_resolve_sibling_crate_visible_functions() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod database; mod artifacts; mod sessions;"),
            (
                "src/database.rs",
                "pub(crate) fn validate_bounded_limit() {}",
            ),
            (
                "src/artifacts.rs",
                "use crate::{database::{validate_bounded_limit as validate_limit}}; pub fn run() { validate_limit(); } mod tests { use super::*; }",
            ),
            (
                "src/sessions.rs",
                "use crate::{database::{validate_bounded_limit as validate_limit}}; pub fn run() { validate_limit(); } pub fn shadowed<validate_limit>() { validate_limit(); } mod tests { use super::*; }",
            ),
        ],
        false,
    );
    for source in ["src/artifacts.rs", "src/sessions.rs"] {
        assert_use(
            &facts,
            (source, "run", "validate_limit", ReferenceKind::Calls),
            ("src/database.rs", "validate_bounded_limit"),
        );
    }
    assert_unresolved(
        &facts,
        (
            "src/sessions.rs",
            "shadowed",
            "validate_limit",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_grouped_super_uses_resolve_sibling_modules_with_nested_tests() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod walk;"),
            ("src/walk.rs", "mod module_system; mod require_aliases;"),
            (
                "src/walk/module_system.rs",
                "use super::{require_aliases}; pub fn run() { require_aliases::collect(); } pub fn shadowed<require_aliases>() { require_aliases::collect(); } mod tests { use super::*; }",
            ),
            ("src/walk/require_aliases.rs", "pub(super) fn collect() {}"),
        ],
        false,
    );
    assert_use(
        &facts,
        (
            "src/walk/module_system.rs",
            "run",
            "require_aliases::collect",
            ReferenceKind::Calls,
        ),
        ("src/walk/require_aliases.rs", "collect"),
    );
    assert_unresolved(
        &facts,
        (
            "src/walk/module_system.rs",
            "shadowed",
            "require_aliases::collect",
            ReferenceKind::Calls,
        ),
    );
}

#[test]
fn rust_uniform_child_modules_precede_external_crates_even_if_unresolved() {
    for child in [true, false] {
        let mut fixtures = vec![
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/app\", \"crates/tools\"]\n",
            ),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ntools = { path = \"../tools\" }\n",
            ),
            (
                "crates/app/src/lib.rs",
                "mod tools; use tools::helper; pub fn run() { helper(); }",
            ),
            (
                "crates/tools/Cargo.toml",
                "[package]\nname = \"tools\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/tools/src/lib.rs", "pub fn helper() {}"),
        ];
        if child {
            fixtures.push(("crates/app/src/tools.rs", "pub(super) fn helper() {}"));
        }
        let facts = build_capability_generation(&fixtures, false);
        if child {
            assert_use(
                &facts,
                (
                    "crates/app/src/lib.rs",
                    "run",
                    "helper",
                    ReferenceKind::Calls,
                ),
                ("crates/app/src/tools.rs", "helper"),
            );
        } else {
            assert_unresolved(
                &facts,
                (
                    "crates/app/src/lib.rs",
                    "run",
                    "helper",
                    ReferenceKind::Calls,
                ),
            );
        }
    }
}

fn assert_use(
    facts: &CanonicalGenerationFacts,
    source: (&str, &str, &str, ReferenceKind),
    target: (&str, &str),
) {
    let (path, owner, name, kind) = source;
    let owner = capability_symbol(facts, path, owner);
    let reference = CapabilityReferenceQuery::new(facts, owner).named(name, kind);
    let target = capability_symbol(facts, target.0, target.1);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{reference:?}"
    );
    assert_eq!(reference.resolution_provenance, "native-import-binding");
    assert_eq!(reference.confidence, 1.0);
}

fn assert_unresolved(facts: &CanonicalGenerationFacts, query: (&str, &str, &str, ReferenceKind)) {
    let (path, owner, name, kind) = query;
    let owner = capability_symbol(facts, path, owner);
    let reference = CapabilityReferenceQuery::new(facts, owner).named(name, kind);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}
