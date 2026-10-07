use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceInput, ReferenceKind,
    build_capability_generation, capability_symbol,
};

pub(super) fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reverse = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reverse.digest());
    assert_eq!(forward.references(), reverse.references());
    assert_eq!(forward.edges(), reverse.edges());
    forward
}

pub(super) fn call<'a>(
    facts: &'a CanonicalGenerationFacts,
    file: &str,
    name: &str,
) -> &'a ReferenceInput {
    CapabilityReferenceQuery::new(
        facts,
        capability_symbol(
            facts,
            file,
            if std::path::Path::new(file)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ex"))
            {
                "Caller.use_it"
            } else {
                "use_it"
            },
        ),
    )
    .named(name, ReferenceKind::Calls)
}

fn named_calls<'a>(
    facts: &'a CanonicalGenerationFacts,
    owner: &super::SymbolInput,
    name: &str,
) -> Vec<&'a ReferenceInput> {
    facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference.reference_name == name
                && reference.reference_kind == "calls"
        })
        .collect()
}

fn assert_call(facts: &CanonicalGenerationFacts, (source, target): (&str, &str), provenance: &str) {
    let reference = call(facts, source, "helper");
    let symbol = capability_symbol(facts, target, "helper");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&symbol.symbol_id));
    assert_eq!(reference.resolution_provenance, provenance);
    assert_eq!(reference.confidence, 1.0);
}

#[test]
fn shell_calls_bind_only_literal_source_edges_and_abstain_on_ambiguous_functions() {
    for (suffix, function) in [
        ("sh", "helper() { :; }\n"),
        ("zsh", "helper() { :; }\n"),
        ("fish", "function helper\nend\n"),
    ] {
        let body = if suffix == "fish" {
            "function use_it\n helper\n missing\nend\n"
        } else {
            "use_it() { helper; missing; }\n"
        };
        let source = format!("source ./lib/helper.{suffix}\n{body}");
        let main = format!("scripts/main.{suffix}");
        let target = format!("lib/helper.{suffix}");
        let other = format!("other/helper.{suffix}");
        let facts = generation(&[(&main, &source), (&target, function), (&other, function)]);
        assert_call(&facts, (&main, &target), "native-shell-source");
        assert!(call(&facts, &main, "missing").target_symbol_id.is_none());

        let ambiguous = format!("source ./lib/helper.{suffix}\n. ./other/helper.{suffix}\n{body}");
        let facts = generation(&[(&main, &ambiguous), (&target, function), (&other, function)]);
        assert!(call(&facts, &main, "helper").target_symbol_id.is_none());

        let dynamic = format!("source $UNKNOWN/helper.{suffix}\n{body}");
        let facts = generation(&[(&main, &dynamic), (&target, function)]);
        assert!(call(&facts, &main, "helper").target_symbol_id.is_none());
    }
}

#[test]
fn powershell_using_module_binds_functions_through_the_exact_relative_path() {
    let facts = generation(&[
        (
            "scripts/main.ps1",
            "using module ../Modules/Helpers.psm1\nfunction use_it { helper; missing }\n",
        ),
        ("Modules/Helpers.psm1", "function helper { }\n"),
        ("other/Helpers.psm1", "function helper { }\n"),
    ]);
    assert_call(
        &facts,
        ("scripts/main.ps1", "Modules/Helpers.psm1"),
        "native-shell-source",
    );
    assert!(
        call(&facts, "scripts/main.ps1", "missing")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn shell_source_calls_do_not_leak_function_scope_or_bind_before_the_load() {
    for source in [
        "setup() { source ./lib/tools.sh; }\nuse_it() { helper; }\n",
        "use_it() { helper; }\nsource ./lib/tools.sh\n",
    ] {
        let facts = generation(&[("main.sh", source), ("lib/tools.sh", "helper() { :; }\n")]);
        assert!(call(&facts, "main.sh", "helper").target_symbol_id.is_none());
    }
    let facts = generation(&[
        (
            "scripts/main.ps1",
            ". ../Modules/Helpers.ps1\nfunction use_it { helper }\n",
        ),
        ("Modules/Helpers.ps1", "function helper { }\n"),
    ]);
    assert_call(
        &facts,
        ("scripts/main.ps1", "Modules/Helpers.ps1"),
        "native-shell-source",
    );
    let facts = generation(&[
        (
            "scripts/main.ps1",
            ". $unknown\nfunction use_it { helper }\n",
        ),
        ("Modules/Helpers.ps1", "function helper { }\n"),
    ]);
    assert!(
        call(&facts, "scripts/main.ps1", "helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_explicit_imports_win_over_unrelated_globs_and_bind_workspace_paths() {
    let facts = generation(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/core\", \"crates/app\"]\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n",
        ),
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[dependencies]\ndemo-core = { path = \"../core\" }\n",
        ),
        (
            "crates/core/src/lib.rs",
            "pub mod helpers;\npub mod consumer;\npub const LIMIT: u32 = 2;\n",
        ),
        (
            "crates/core/src/helpers.rs",
            "pub fn helper() {}\npub struct Thing;\nimpl Thing { pub fn new() -> Self { Thing } }\n",
        ),
        (
            "crates/core/src/consumer/mod.rs",
            "use crate::helpers::{helper, Thing};\nuse super::LIMIT;\nuse std::sync::*;\npub fn use_it() { helper(); let _ = Thing::new(); let _ = LIMIT; }\n",
        ),
        (
            "crates/app/src/lib.rs",
            "use demo_core::helpers::{helper, Thing};\npub fn use_it() { helper(); Thing::new(); demo_core::helpers::helper(); missing::helper(); }\n",
        ),
    ]);
    for source in ["crates/core/src/consumer/mod.rs", "crates/app/src/lib.rs"] {
        let provenance = if source.contains("consumer") {
            "native-import-binding"
        } else {
            "native-rust-workspace-crate"
        };
        assert_call(&facts, (source, "crates/core/src/helpers.rs"), provenance);
        let reference = call(&facts, source, "Thing::new");
        let target = capability_symbol(&facts, "crates/core/src/helpers.rs", "Thing::new");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, provenance);
    }
    assert!(
        call(&facts, "crates/app/src/lib.rs", "missing::helper")
            .target_symbol_id
            .is_none()
    );
    let owner = capability_symbol(&facts, "crates/core/src/consumer/mod.rs", "use_it");
    let constant =
        CapabilityReferenceQuery::new(&facts, owner).named("LIMIT", ReferenceKind::References);
    assert_eq!(
        constant.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "crates/core/src/lib.rs", "LIMIT").symbol_id)
    );
    assert_eq!(constant.resolution_provenance, "native-import-binding");
}

#[test]
fn rust_ambiguous_workspace_crates_and_inaccessible_members_stay_unresolved() {
    let facts = generation(&[
        (
            "a/Cargo.toml",
            "[package]\nname = \"same\"\nversion = \"1.0.0\"\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"same\"\nversion = \"1.0.0\"\n",
        ),
        ("a/src/lib.rs", "pub mod helpers;\n"),
        ("a/src/helpers.rs", "pub fn helper() {}\n"),
        ("b/src/lib.rs", "pub mod helpers;\n"),
        ("b/src/helpers.rs", "pub fn helper() {}\n"),
        (
            "app/src/main.rs",
            "use same::helpers::helper;\nfn use_it() { helper(); }\n",
        ),
    ]);
    assert!(
        call(&facts, "app/src/main.rs", "helper")
            .target_symbol_id
            .is_none()
    );
    let facts = generation(&[
        ("src/lib.rs", "mod helpers;\nmod consumer;\n"),
        ("src/helpers.rs", "fn helper() {}\n"),
        (
            "src/consumer.rs",
            "use crate::helpers::helper;\nfn use_it() { helper(); }\n",
        ),
    ]);
    assert!(
        call(&facts, "src/consumer.rs", "helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_workspace_names_require_a_local_path_dependency_and_never_shadow_registry_crates() {
    for dependencies in [
        "",
        "[dependencies]\ndemo-core = \"1\"\n",
        "[dependencies]\ndemo-core = { path = \"../other\" }\n",
    ] {
        let manifest = format!("[package]\nname = \"app\"\nversion = \"1.0.0\"\n{dependencies}");
        let facts = generation(&[
            (
                "core/Cargo.toml",
                "[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n",
            ),
            (
                "core/src/lib.rs",
                "pub mod helpers; pub mod inline { pub fn helper() {} }\n",
            ),
            ("core/src/helpers.rs", "pub fn helper() {}\n"),
            ("app/Cargo.toml", &manifest),
            (
                "app/src/lib.rs",
                "use demo_core::helpers::helper; fn use_it() { helper(); demo_core::helpers::helper(); demo_core::inline::helper(); }\n",
            ),
        ]);
        for name in [
            "helper",
            "demo_core::helpers::helper",
            "demo_core::inline::helper",
        ] {
            assert!(
                call(&facts, "app/src/lib.rs", name)
                    .target_symbol_id
                    .is_none(),
                "{dependencies}: {name}"
            );
        }
    }
}

#[test]
fn rust_renamed_literal_path_dependencies_bind_their_exact_local_crate() {
    let facts = generation(&[
        (
            "core/Cargo.toml",
            "[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n",
        ),
        ("core/src/lib.rs", "pub fn helper() {}\n"),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[dependencies]\nselected = { package = \"demo-core\", path = \"../core\" }\n",
        ),
        (
            "app/src/lib.rs",
            "use selected::helper; fn use_it() { helper(); selected::helper(); }\n",
        ),
    ]);
    assert_call(
        &facts,
        ("app/src/lib.rs", "core/src/lib.rs"),
        "native-rust-workspace-crate",
    );
    let reference = call(&facts, "app/src/lib.rs", "selected::helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "core/src/lib.rs", "helper").symbol_id)
    );
    assert_eq!(
        reference.resolution_provenance,
        "native-rust-workspace-crate"
    );
}

#[test]
fn rust_path_dependencies_do_not_select_orphan_default_libraries_with_a_custom_entry() {
    for configuration in [
        "[lib]\npath = \"src/actual.rs\"\n",
        "[lib]\nname = \"different\"\n",
        "",
    ] {
        let manifest =
            format!("[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n{configuration}");
        let facts = generation(&[
            ("core/Cargo.toml", &manifest),
            ("core/src/lib.rs", "pub fn helper() {}\n"),
            ("core/src/actual.rs", "pub fn helper() {}\n"),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[dependencies]\ndemo-core = { path = \"../core\" }\n",
            ),
            ("app/src/lib.rs", "fn use_it() { demo_core::helper(); }\n"),
        ]);
        let reference = call(&facts, "app/src/lib.rs", "demo_core::helper");
        if configuration.is_empty() {
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&capability_symbol(&facts, "core/src/lib.rs", "helper").symbol_id)
            );
            assert_eq!(
                reference.resolution_provenance,
                "native-rust-workspace-crate"
            );
        } else {
            assert!(reference.target_symbol_id.is_none());
        }
    }
}

#[test]
fn rust_unproven_custom_and_binary_paths_preserve_base_resolution() {
    for (manifest, source) in [
        (
            "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n[lib]\npath = \"src/actual.rs\"\n",
            "src/actual.rs",
        ),
        (
            "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
            "src/bin/tool.rs",
        ),
    ] {
        let facts = generation(&[
            ("Cargo.toml", manifest),
            (
                "src/lib.rs",
                "mod helpers; pub mod inline { pub fn helper() {} }\n",
            ),
            ("src/helpers.rs", "pub fn helper() {}\n"),
            (
                source,
                "fn use_it() { crate::helpers::helper(); crate::inline::helper(); }\n",
            ),
        ]);
        for name in ["crate::helpers::helper", "crate::inline::helper"] {
            let target = if name.contains("inline") {
                ("src/lib.rs", "inline::helper")
            } else {
                ("src/helpers.rs", "helper")
            };
            assert_base_rust_target(&facts, (source, name), target);
        }
    }
}

#[test]
fn rust_integration_tests_bind_their_own_conventional_library_through_the_explicit_import() {
    let facts = generation(&[
        (
            "core/Cargo.toml",
            "[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n",
        ),
        ("core/src/lib.rs", "pub mod helpers;\n"),
        ("core/src/helpers.rs", "pub fn helper() {}\n"),
        (
            "core/tests/check.rs",
            "use demo_core::helpers::helper; fn use_it() { helper(); }\n",
        ),
        (
            "unrelated/tests/check.rs",
            "use demo_core::helpers::helper; fn use_it() { helper(); }\n",
        ),
    ]);
    assert_call(
        &facts,
        ("core/tests/check.rs", "core/src/helpers.rs"),
        "native-rust-workspace-crate",
    );
    assert!(
        call(&facts, "unrelated/tests/check.rs", "helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_nested_packages_never_inherit_the_outer_integration_tests_library() {
    let source = "core/tests/nested/src/bin/tool.rs";
    let facts = generation(&[
        (
            "core/Cargo.toml",
            "[package]\nname = \"demo-core\"\nversion = \"1.0.0\"\n",
        ),
        ("core/src/lib.rs", "pub mod helpers;\n"),
        ("core/src/helpers.rs", "pub fn helper() {}\n"),
        (
            "core/tests/nested/Cargo.toml",
            "[package]\nname = \"nested\"\nversion = \"1.0.0\"\n[dependencies]\ndemo-core = \"1\"\n",
        ),
        (
            source,
            "use demo_core::helpers::helper; fn use_it() { helper(); demo_core::helpers::helper(); }\n",
        ),
    ]);
    for name in ["helper", "demo_core::helpers::helper"] {
        assert!(call(&facts, source, name).target_symbol_id.is_none());
    }
}

#[test]
fn rust_unproven_module_edges_abstain_without_suppressing_base() {
    for root in [
        "mod consumer;\n",
        "#[path = \"other.rs\"]\n#[doc = \"helper\"]\npub mod helpers;\nmod consumer;\n",
    ] {
        let facts = generation(&[
            ("src/lib.rs", root),
            ("src/helpers.rs", "pub fn helper() {}\n"),
            ("src/other.rs", "pub fn helper() {}\n"),
            (
                "src/consumer.rs",
                "fn use_it() { crate::helpers::helper(); }\n",
            ),
        ]);
        assert_base_rust_target(
            &facts,
            ("src/consumer.rs", "crate::helpers::helper"),
            ("src/helpers.rs", "helper"),
        );
    }
    let facts = generation(&[
        ("src/lib.rs", "pub mod helpers;\nmod consumer;\n"),
        ("src/helpers.rs", "pub fn helper() {}\n"),
        ("src/helpers/mod.rs", "pub fn helper() {}\n"),
        (
            "src/consumer.rs",
            "fn use_it() { crate::helpers::helper(); }\n",
        ),
    ]);
    assert!(
        call(&facts, "src/consumer.rs", "crate::helpers::helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_private_child_module_proof_abstains_to_base_outside_the_subtree() {
    let facts = generation(&[
        ("src/lib.rs", "mod outer;\nmod consumer;\n"),
        (
            "src/outer/mod.rs",
            "mod hidden;\npub fn use_it() { self::hidden::helper(); }\n",
        ),
        ("src/outer/hidden.rs", "pub fn helper() {}\n"),
        (
            "src/consumer.rs",
            "fn use_it() { crate::outer::hidden::helper(); }\n",
        ),
    ]);
    let reference = call(&facts, "src/outer/mod.rs", "self::hidden::helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "src/outer/hidden.rs", "helper").symbol_id)
    );
    assert_eq!(
        reference.resolution_provenance,
        "native-rust-qualified-path"
    );
    assert_base_rust_target(
        &facts,
        ("src/consumer.rs", "crate::outer::hidden::helper"),
        ("src/outer/hidden.rs", "helper"),
    );
}

#[test]
fn c_family_calls_prefer_the_unique_matching_definition_and_never_choose_a_namesake() {
    let facts = generation(&[
        (
            "kernels/main.cu",
            "void helper(int n);\nvoid use_it() { helper(1); missing(1); }\n",
        ),
        ("kernels/helper.cu", "void helper(int n) {}\n"),
        ("kernels/other.cu", "void helper(float n) {}\n"),
    ]);
    let reference = call(&facts, "kernels/main.cu", "helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "kernels/helper.cu", "helper").symbol_id)
    );
    assert_eq!(
        reference.resolution_provenance,
        "native-c-declaration-definition"
    );
    assert_eq!(reference.confidence, 0.95);
    assert!(
        call(&facts, "kernels/main.cu", "missing")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn osiris_explicit_goal_self_references_retain_the_exact_reference_and_edge() {
    let path = "Mods/Demo/Story/RawFiles/Goals/Init.txt";
    let facts = generation(&[(
        path,
        "INITSECTION\nEXITSECTION\nSysCompleteGoal(\"Init\");\nSysCompleteGoal(\"Missing\");\nENDEXITSECTION\n",
    )]);
    let goal = capability_symbol(&facts, path, "Init");
    let reference =
        CapabilityReferenceQuery::new(&facts, goal).named("Init", ReferenceKind::References);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&goal.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-resource-self-reference"
    );
    assert_eq!(reference.confidence, 1.0);
    assert!(
        facts
            .edges()
            .iter()
            .any(|edge| edge.source_symbol_id == goal.symbol_id
                && edge.target_symbol_id == goal.symbol_id
                && edge.kind == super::EdgeKind::References)
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, goal)
            .named("Missing", ReferenceKind::References)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn elixir_module_calls_keep_the_qualified_callee_and_scoped_alias_target() {
    let facts = generation(&[
        (
            "lib/shop/tools.ex",
            "defmodule Shop.Tools do\n def helper(), do: :ok\n defp private(), do: :ok\nend\n",
        ),
        (
            "lib/app.ex",
            "defmodule App do\n alias Shop.Tools\n def use_it() do\n Tools.helper()\n Shop.Tools.helper()\n Tools.private()\n Other.Tools.helper()\n end\nend\n",
        ),
        (
            "lib/other.ex",
            "defmodule Other do\n def helper(), do: :ok\nend\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "lib/app.ex", "App.use_it");
    let target = capability_symbol(&facts, "lib/shop/tools.ex", "Shop.Tools.helper");
    let references = named_calls(&facts, owner, "helper");
    assert_eq!(references.len(), 3);
    assert_eq!(
        references
            .iter()
            .filter(|reference| reference.target_symbol_id.as_ref() == Some(&target.symbol_id))
            .count(),
        2
    );
    for reference in references
        .iter()
        .filter(|reference| reference.target_symbol_id.is_some())
    {
        assert_eq!(reference.resolution_provenance, "native-elixir-module");
        assert_eq!(reference.confidence, 1.0);
    }
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("private", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn elixir_aliases_bind_static_as_names_and_stay_inside_their_scope() {
    let facts = generation(&[
        (
            "tools.ex",
            "defmodule Shop.Tools do\n def helper(), do: :ok\nend\n",
        ),
        (
            "main.ex",
            "defmodule App do\n alias Shop.Tools, as: Pick\n def use_it(), do: Pick.helper()\nend\ndefmodule Outside do\n def use_it(), do: Pick.helper()\nend\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "main.ex", "App.use_it");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "tools.ex", "Shop.Tools.helper").symbol_id)
    );
    assert_eq!(reference.resolution_provenance, "native-elixir-module");
    let outside = capability_symbol(&facts, "main.ex", "Outside.use_it");
    assert!(
        CapabilityReferenceQuery::new(&facts, outside)
            .named("helper", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn qualified_callees_never_import_an_unqualified_function_into_the_caller() {
    let (source, target, owner, module, qualified, bare) = (
        "caller.ex",
        "foo.ex",
        "Caller.use_it",
        "defmodule Foo do\n def helper(), do: :ok\nend\n",
        "defmodule Caller do\n def use_it() do\n Foo.helper()\n helper()\n end\nend\n",
        "defmodule Caller do\n def use_it(), do: helper()\nend\n",
    );
    let facts = generation(&[(source, qualified), (target, module)]);
    let owner_symbol = capability_symbol(&facts, source, owner);
    let mut calls = named_calls(&facts, owner_symbol, "helper");
    calls.sort_by_key(|reference| reference.start_byte);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, target, "Foo.helper").symbol_id)
    );
    assert_eq!(calls[0].resolution_provenance, "native-elixir-module");
    assert!(calls[1].target_symbol_id.is_none());
    let facts = generation(&[(source, bare), (target, module)]);
    assert!(
        named_calls(&facts, capability_symbol(&facts, source, owner), "helper")[0]
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn elixir_remote_private_calls_abstain_to_existing_lexical_resolution() {
    let facts = generation(&[(
        "tools.ex",
        "defmodule Tools do\n defp hidden(), do: :ok\n def use_it(), do: Tools.hidden()\nend\n",
    )]);
    let owner = capability_symbol(&facts, "tools.ex", "Tools.use_it");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("hidden", ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "tools.ex", "Tools.hidden").symbol_id)
    );
    assert_eq!(reference.resolution_provenance, "native-exact-lexical");
}

#[test]
fn elixir_nested_module_names_never_bind_an_unrelated_global_namesake() {
    let facts = generation(&[
        (
            "outer.ex",
            "defmodule Outer do\n defmodule Inner do\n  def helper(), do: :nested\n end\n def use_it(), do: Inner.helper()\nend\n",
        ),
        (
            "other.ex",
            "defmodule Inner do\n def helper(), do: :global\nend\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "outer.ex", "Outer.use_it");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("helper", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn go_qualified_calls_require_the_full_import_path_and_unique_exported_member() {
    let facts = generation(&[
        (
            "cmd/main.go",
            "package main\nimport p \"example.com/shop/tools\"\nfunc use_it() { p.Helper(); p.private(); }\n",
        ),
        (
            "example.com/shop/tools/tools.go",
            "package tools\nfunc Helper() {}\nfunc private() {}\n",
        ),
        ("other/tools/tools.go", "package tools\nfunc Helper() {}\n"),
    ]);
    let reference = call(&facts, "cmd/main.go", "p.Helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "example.com/shop/tools/tools.go", "Helper").symbol_id)
    );
    assert_eq!(reference.resolution_provenance, "native-go-import-path");
    assert_eq!(reference.confidence, 1.0);
    assert!(
        call(&facts, "cmd/main.go", "p.private")
            .target_symbol_id
            .is_none()
    );
    let ambiguous = generation(&[
        (
            "cmd/main.go",
            "package main\nimport p \"example.com/shop/tools\"\nfunc use_it() { p.Helper(); }\n",
        ),
        (
            "example.com/shop/tools/a.go",
            "package tools\nfunc Helper() {}\n",
        ),
        (
            "example.com/shop/tools/b.go",
            "package tools\nfunc Helper() {}\n",
        ),
    ]);
    assert!(
        call(&ambiguous, "cmd/main.go", "p.Helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn go_qualified_calls_do_not_guess_a_module_prefix_or_default_package_name() {
    for (target, package) in [
        ("internal/tools/tools.go", "tools"),
        ("example.com/shop/tools/tools.go", "different"),
    ] {
        let body = format!("package {package}\nfunc Helper() {{}}\n");
        let facts = generation(&[
            (
                "main.go",
                "package main\nimport \"example.com/shop/tools\"\nfunc use_it() { tools.Helper() }\n",
            ),
            (target, &body),
        ]);
        assert!(
            call(&facts, "main.go", "tools.Helper")
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn go_shadowed_namespace_aliases_never_bind_the_imported_package() {
    let facts = generation(&[
        (
            "main.go",
            "package main\nimport p \"example.com/shop/tools\"\ntype Thing struct{}\nfunc use_it(p Thing) { p.Helper() }\n",
        ),
        (
            "example.com/shop/tools/tools.go",
            "package tools\nfunc Helper() {}\n",
        ),
    ]);
    assert!(
        call(&facts, "main.go", "p.Helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn go_generic_type_parameters_shadow_namespace_aliases() {
    let facts = generation(&[
        (
            "main.go",
            "package main\nimport p \"example.com/shop/tools\"\nfunc use_it[p any]() { p.Helper() }\n",
        ),
        (
            "example.com/shop/tools/tools.go",
            "package tools\nfunc Helper() {}\n",
        ),
    ]);
    assert!(
        call(&facts, "main.go", "p.Helper")
            .target_symbol_id
            .is_none()
    );
}

fn assert_base_rust_target(
    facts: &CanonicalGenerationFacts,
    query: (&str, &str),
    target: (&str, &str),
) {
    let reference = call(facts, query.0, query.1);
    let symbol = capability_symbol(facts, target.0, target.1);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&symbol.symbol_id));
    assert_eq!(reference.confidence, 1.0);
    assert_eq!(
        reference.resolution_provenance,
        "native-rust-qualified-path"
    );
}
