//! Adversarial repair cases exercise extraction and the real native pipeline.
use super::unqual_resolution::{call, generation};
use super::{
    CanonicalGenerationFacts, FULL_TEST_EVIDENCE, NativeClonePolicy, NativeExtractor,
    NativeFactAccumulator, PipelineStage, ReferenceInput, ResolveGenerationRequest, SourceLimits,
    TEST_GENERATION_BYTES, TEST_SOURCE_BYTES, capability_symbol, generation_validation_limits,
    resolve_generation, test_source_root, validate_generation_facts,
};
use std::fmt::Write;

fn unresolved(facts: &CanonicalGenerationFacts, file: &str, name: &str) {
    assert!(
        call(facts, file, name).target_symbol_id.is_none(),
        "{file}: {name}"
    );
}

fn same_base_resolution(actual: &ReferenceInput, base: &ReferenceInput) {
    assert_eq!(actual.target_symbol_id, base.target_symbol_id);
    assert_eq!(actual.confidence, base.confidence);
    assert_eq!(actual.resolution_provenance, base.resolution_provenance);
}

#[test]
fn repair_01_ocaml_open_never_proves_a_global_compilation_unit() {
    for prefix in ["open M\n", "include M\n", ""] {
        let source = format!("{prefix}let use_it () = X.helper ()\n");
        let facts = generation(&[
            ("m.ml", "module X = struct let helper () = 1 end\n"),
            ("x.ml", "let helper () = 2\n"),
            ("caller.ml", &source),
        ]);
        unresolved(&facts, "caller.ml", "helper");
    }
}

#[test]
fn repair_02_elixir_aliases_built_from_earlier_aliases_fence_the_scope() {
    for alias in [
        "alias A.Tools, as: T",
        "require First, as: A\nalias A.Tools, as: T",
    ] {
        let source = format!(
            "defmodule Caller do\n alias First, as: A\n {alias}\n def use_it(), do: T.helper()\nend\n"
        );
        let facts = generation(&[
            (
                "first.ex",
                "defmodule First.Tools do\n def helper(), do: :first\nend\n",
            ),
            ("a.ex", "defmodule A.Tools do\n def helper(), do: :a\nend\n"),
            ("caller.ex", &source),
        ]);
        unresolved(&facts, "caller.ex", "helper");
    }
}

#[test]
fn repair_03_unsupported_elixir_alias_forms_fence_all_calls_in_the_scope() {
    for alias in [
        "alias Foo.{Bar, Baz}",
        "alias __MODULE__.Bar",
        "alias [Foo.Bar, Foo.Baz]",
        "alias module_name()",
    ] {
        let source =
            format!("defmodule Caller do\n def use_it(), do: Bar.helper()\n {alias}\nend\n");
        let facts = generation(&[
            (
                "foo.ex",
                "defmodule Foo.Bar do\n def helper(), do: :foo\nend\n",
            ),
            ("bar.ex", "defmodule Bar do\n def helper(), do: :bar\nend\n"),
            ("caller.ex", &source),
        ]);
        unresolved(&facts, "caller.ex", "helper");
    }
}

#[test]
fn repair_04_elixir_private_overloads_withdraw_remote_proof_and_keep_base() {
    for definitions in [
        "def helper(x), do: x\n defp helper(), do: :private",
        "def helper(x), do: x\n def helper(), do: :other",
    ] {
        let tools = format!("defmodule Tools do\n {definitions}\nend\n");
        let facts = generation(&[
            ("tools.ex", &tools),
            (
                "caller.ex",
                "defmodule Caller do\n def use_it(), do: Tools.helper()\nend\n",
            ),
        ]);
        let base = generation(&[
            ("tools.ex", &tools),
            (
                "caller.ex",
                "defmodule Caller do\n def use_it(), do: helper()\nend\n",
            ),
        ]);
        same_base_resolution(
            call(&facts, "caller.ex", "helper"),
            call(&base, "caller.ex", "helper"),
        );
    }
}

#[test]
fn repair_05_lua_factory_and_environment_writes_withdraw_all_require_proofs() {
    for extension in ["lua", "luau"] {
        for mutation in [
            "_G.require = function(_) return {helper = replacement} end",
            "_G[\"require\"] = function(_) return {helper = replacement} end",
            "_G['require'] = replacement",
            "require = replacement",
            "_ENV = {}",
            "setfenv(1, {})",
            "rawset(_G, \"require\", replacement)",
        ] {
            let source = format!(
                "local function replacement() end\n{mutation}\nlocal api = require('lib.tools')\nlocal function use_it() api.helper() end\n"
            );
            let main = format!("main.{extension}");
            let tools = format!("lib/tools.{extension}");
            let facts = generation(&[
                (&main, &source),
                (&tools, "local M = {}\nfunction M.helper() end\nreturn M\n"),
            ]);
            unresolved(&facts, &main, "api.helper");
        }
    }
}

#[test]
fn repair_06_powershell_dot_source_abstains_on_distinct_working_directory_targets() {
    let facts = generation(&[
        (
            "scripts/main.ps1",
            ". ./helpers.ps1\nfunction use_it { helper }\nuse_it\n",
        ),
        ("helpers.ps1", "function helper { 'root' }\n"),
        ("scripts/helpers.ps1", "function helper { 'script' }\n"),
    ]);
    unresolved(&facts, "scripts/main.ps1", "helper");
}

#[test]
fn repair_07_directory_changes_before_source_withdraw_the_load() {
    for command in ["cd sub", "pushd sub", "popd", "chdir sub"] {
        let source = format!("{command}\nsource ./tools.sh\nuse_it() {{ helper; }}\nuse_it\n");
        let facts = generation(&[
            ("main.sh", &source),
            ("tools.sh", "helper() { echo root; }\n"),
            ("sub/tools.sh", "helper() { echo sub; }\n"),
        ]);
        unresolved(&facts, "main.sh", "helper");
    }
    for command in [
        "Set-Location sub",
        "Push-Location sub",
        "Pop-Location",
        "sl sub",
    ] {
        let source = format!("{command}\n. ./tools.ps1\nfunction use_it {{ helper }}\n");
        let facts = generation(&[("main.ps1", &source), ("tools.ps1", "function helper {}\n")]);
        unresolved(&facts, "main.ps1", "helper");
    }
}

#[test]
fn repair_08_source_in_nested_execution_context_never_creates_a_file_binding() {
    for load in [
        "(source ./lib/tools.sh)",
        "loaded=$(source ./lib/tools.sh)",
        "source ./lib/tools.sh | cat",
        "true && source ./lib/tools.sh",
        "false || source ./lib/tools.sh",
        "if true; then source ./lib/tools.sh; fi",
        "while false; do source ./lib/tools.sh; done",
        "setup() { source ./lib/tools.sh; }",
    ] {
        let source = format!("{load}\nuse_it() {{ helper; }}\nuse_it\n");
        let facts = generation(&[("main.sh", &source), ("lib/tools.sh", "helper() { :; }\n")]);
        unresolved(&facts, "main.sh", "helper");
    }
}

fn assert_rust_helper(facts: &CanonicalGenerationFacts) {
    let target = capability_symbol(facts, "src/helpers.rs", "helper");
    let reference = call(facts, "src/consumer.rs", "crate::helpers::helper");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.confidence, 1.0);
    assert_eq!(
        reference.resolution_provenance,
        "native-rust-qualified-path"
    );
}

fn rust_fixture(manifest: &str, binary: &str) -> CanonicalGenerationFacts {
    generation(&[
        ("Cargo.toml", manifest),
        ("src/lib.rs", "pub mod helpers; mod consumer;\n"),
        ("src/main.rs", binary),
        ("src/helpers.rs", "pub fn helper() {}\n"),
        (
            "src/consumer.rs",
            "fn use_it() { crate::helpers::helper(); }\n",
        ),
    ])
}

#[test]
fn repair_09_library_and_binary_root_ownership_preserves_exact_base_paths() {
    let manifest = "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n";
    assert_rust_helper(&rust_fixture(manifest, "fn main() {}\n"));
    // Two declared roots reach consumer; the new path abstains and base still resolves.
    assert_rust_helper(&rust_fixture(manifest, "mod consumer; fn main() {}\n"));
    let facts = generation(&[
        ("Cargo.toml", manifest),
        ("src/lib.rs", "mod consumer;\n"),
        (
            "src/consumer.rs",
            "fn use_it() { crate::missing::helper(); }\n",
        ),
        ("other.rs", "pub fn helper() {}\n"),
    ]);
    unresolved(&facts, "src/consumer.rs", "crate::missing::helper");
}

#[test]
fn repair_10_unrelated_cargo_settings_preserve_the_conventional_library() {
    for settings in [
        "autotests = false",
        "autobenches = false",
        "autoexamples = false",
        "edition = \"2024\"",
    ] {
        let manifest = format!("[package]\nname = \"demo\"\nversion = \"1.0.0\"\n{settings}\n");
        let facts = generation(&[
            ("Cargo.toml", &manifest),
            ("src/lib.rs", "pub mod helpers; mod consumer;\n"),
            ("src/helpers.rs", "pub fn helper() {}\n"),
            (
                "src/consumer.rs",
                "fn use_it() { crate::helpers::helper(); }\n",
            ),
        ]);
        assert_rust_helper(&facts);
    }
}

fn extracted_generation(fixtures: &[(&str, &str)]) -> NativeFactAccumulator {
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limits: {error}"));
    let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
    for (path, source) in fixtures {
        let file = fixture_extraction((path, source), limits);
        accumulator
            .push(file)
            .unwrap_or_else(|_| panic!("fixture input budget"));
    }
    accumulator
}

fn fixture_index(fixtures: &[(&str, &str)]) -> super::ResolutionIndex {
    let accumulator = extracted_generation(fixtures);
    let mut budget = super::ResolveBudget::new(0, TEST_GENERATION_BYTES)
        .unwrap_or_else(|_| panic!("fixture index budget"));
    let source_root = test_source_root();
    super::build_resolution_index(
        &accumulator,
        super::ResolutionIndexContext {
            source_root: &source_root,
            budget: &mut budget,
            cancelled: &mut || false,
        },
    )
    .unwrap_or_else(|_| panic!("fixture indexing"))
}

fn measured_generation(fixtures: &[(&str, &str)]) -> (CanonicalGenerationFacts, usize) {
    let accumulator = extracted_generation(fixtures);
    let mut polls = 0;
    let (facts, _) = resolve_generation(
        ResolveGenerationRequest {
            extracted: accumulator,
            maximum_bytes: TEST_GENERATION_BYTES,
            source_root: test_source_root(),
            evidence_policy: FULL_TEST_EVIDENCE,
            clone_policy: NativeClonePolicy {
                wider_partial_band: false,
            },
        },
        || {
            polls += 1;
            false
        },
    )
    .unwrap_or_else(|_| panic!("fixture resolution budget"));
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("validation limits: {error}"));
    let (facts, _) = validate_generation_facts(facts, limits, || false)
        .unwrap_or_else(|error| panic!("fact validation: {error}"));
    (facts, polls)
}

fn assert_linear_work(samples: [usize; 3]) {
    let [small, medium, large] = samples;
    assert!(
        large - medium <= (medium - small) * 2 + 64,
        "nonlinear cancellation-polled work: {samples:?}"
    );
}

#[test]
fn repair_11_repeated_source_loads_and_calls_have_linear_resolution_work() {
    let mut samples = [0; 3];
    for (slot, count) in [128, 256, 512].into_iter().enumerate() {
        let source = format!(
            "{}use_it() {{ {} }}\n",
            "source ./lib/tools.sh\n".repeat(count),
            "helper; ".repeat(count)
        );
        let (facts, polls) =
            measured_generation(&[("main.sh", &source), ("lib/tools.sh", "helper() { :; }\n")]);
        let target = capability_symbol(&facts, "lib/tools.sh", "helper");
        let calls: Vec<_> = facts
            .references()
            .iter()
            .filter(|reference| {
                reference.reference_name == "helper" && reference.reference_kind == "calls"
            })
            .collect();
        assert_eq!(calls.len(), count);
        for reference in calls {
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
            assert_eq!(reference.resolution_provenance, "native-shell-source");
        }
        samples[slot] = polls;
    }
    assert_linear_work(samples);
    let facts = generation(&[
        (
            "main.sh",
            "source ./a.sh\nsource ./b.sh\nuse_it() { helper; }\n",
        ),
        ("a.sh", "helper() { :; }\n"),
        ("b.sh", "helper() { :; }\n"),
    ]);
    unresolved(&facts, "main.sh", "helper");
}

#[test]
fn repair_12_reexport_facade_lookup_has_linear_resolution_work() {
    let mut samples = [0; 3];
    for (slot, count) in [128, 256, 512].into_iter().enumerate() {
        let source = facade_source(count);
        let (facts, polls) = measured_generation(&[
            ("src/lib.rs", &source),
            ("src/helpers.rs", "pub fn helper() {}\n"),
        ]);
        let target = capability_symbol(&facts, "src/helpers.rs", "helper");
        for i in 0..count {
            let name = format!("crate::H_{i}");
            let reference = call(&facts, "src/lib.rs", &name);
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
            assert_eq!(
                reference.resolution_provenance,
                "native-rust-qualified-path"
            );
        }
        samples[slot] = polls;
    }
    assert_linear_work(samples);
    let facts = generation(&[
        (
            "src/lib.rs",
            "pub mod a; pub mod b; pub use crate::a::helper as H; pub use crate::b::helper as H; fn use_it() { crate::H(); }\n",
        ),
        ("src/a.rs", "pub fn helper() {}\n"),
        ("src/b.rs", "pub fn helper() {}\n"),
    ]);
    unresolved(&facts, "src/lib.rs", "crate::H");
}

#[test]
fn rust_custom_library_and_binary_targets_follow_their_declared_modules() {
    for (target, settings, helper) in [
        (
            "src/actual.rs",
            "[lib]\npath = \"src/actual.rs\"\n",
            "src/helpers.rs",
        ),
        ("src/bin/tool.rs", "", "src/bin/helpers.rs"),
        ("src/bin/tool/main.rs", "", "src/bin/tool/helpers.rs"),
        (
            "app/run.rs",
            "[[bin]]\nname = \"app\"\npath = \"app/run.rs\"\n",
            "app/helpers.rs",
        ),
    ] {
        let manifest = format!("[package]\nname = \"demo\"\nversion = \"1.0.0\"\n{settings}");
        let facts = generation(&[
            ("Cargo.toml", &manifest),
            ("src/lib.rs", "pub mod other;\n"),
            ("src/other.rs", "pub fn helper() {}\n"),
            (
                target,
                "pub mod helpers; fn use_it() { crate::helpers::helper(); }\n",
            ),
            (helper, "pub fn helper() {}\n"),
        ]);
        let reference = call(&facts, target, "crate::helpers::helper");
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&capability_symbol(&facts, helper, "helper").symbol_id)
        );
        assert_eq!(
            reference.resolution_provenance,
            "native-rust-qualified-path"
        );
        assert_eq!(reference.confidence, 1.0);
    }
}

#[test]
fn unqual_production_functions_respect_code_health_limits() {
    let modules = [
        ("reference_tiers.rs", include_str!("../reference_tiers.rs")),
        (
            "rust_use_bindings.rs",
            include_str!("../rust_use_bindings.rs"),
        ),
        (
            "rust_local_types.rs",
            include_str!("../rust_local_types.rs"),
        ),
        (
            "rust_use_guards.rs",
            include_str!("../../../../cartograph-extract/src/walk/polyglot/rust_use_guards.rs"),
        ),
        (
            "declaration_resolution.rs",
            include_str!("../declaration_resolution.rs"),
        ),
        (
            "explicit_edge_resolution.rs",
            include_str!("../explicit_edge_resolution.rs"),
        ),
        (
            "go_path_resolution.rs",
            include_str!("../go_path_resolution.rs"),
        ),
        (
            "module_call_resolution.rs",
            include_str!("../module_call_resolution.rs"),
        ),
        (
            "resource_resolution.rs",
            include_str!("../resource_resolution.rs"),
        ),
        (
            "rust_dependency_paths.rs",
            include_str!("../rust_dependency_paths.rs"),
        ),
        (
            "rust_facade_resolution.rs",
            include_str!("../rust_facade_resolution.rs"),
        ),
        (
            "rust_path_resolution.rs",
            include_str!("../rust_path_resolution.rs"),
        ),
        (
            "rust_root_ownership.rs",
            include_str!("../rust_root_ownership.rs"),
        ),
        (
            "rust_inline_modules.rs",
            include_str!("../rust_inline_modules.rs"),
        ),
        (
            "shell_resolution.rs",
            include_str!("../shell_resolution.rs"),
        ),
        (
            "module_bindings.rs",
            include_str!("../../../../cartograph-extract/src/tags/module_bindings.rs"),
        ),
        (
            "cargo_path_bindings.rs",
            include_str!("../../../../cartograph-extract/src/framework/cargo_path_bindings.rs"),
        ),
        (
            "namespace_bindings.rs",
            include_str!("../../../../cartograph-extract/src/walk/namespace_bindings.rs"),
        ),
        (
            "source_bindings.rs",
            include_str!("../../../../cartograph-extract/src/walk/source_bindings.rs"),
        ),
    ];
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limits: {error}"));
    let mut issues = Vec::new();
    for (path, source) in modules.into_iter().chain(rust_use_module_sources()) {
        let extracted = fixture_extraction((path, source), limits);
        for symbol in extracted.symbols.iter().filter(|symbol| {
            matches!(
                symbol.kind,
                super::SymbolKind::Function | super::SymbolKind::Method
            )
        }) {
            let lines = symbol.span.end_line() - symbol.span.start_line() + 1;
            if symbol.health.cyclomatic > 15 || symbol.health.parameter_count > 3 || lines >= 100 {
                issues.push(format!(
                    "{path}:{} {} cc={} params={} lines={lines}",
                    symbol.span.start_line(),
                    symbol.qualified_name,
                    symbol.health.cyclomatic,
                    symbol.health.parameter_count
                ));
            }
        }
    }
    assert!(issues.is_empty(), "{}", issues.join("\n"));
}

fn rust_use_module_sources() -> [(&'static str, &'static str); 5] {
    [
        (
            "rust_module_scopes.rs",
            include_str!("../../../../cartograph-extract/src/walk/polyglot/rust_module_scopes.rs"),
        ),
        (
            "rust_pattern_guards.rs",
            include_str!("../../../../cartograph-extract/src/walk/polyglot/rust_pattern_guards.rs"),
        ),
        (
            "rust_uniform_paths.rs",
            include_str!("../rust_uniform_paths.rs"),
        ),
        (
            "rust_path_visibility.rs",
            include_str!("../rust_path_visibility.rs"),
        ),
        (
            "rust_scoped_modules.rs",
            include_str!("../rust_scoped_modules.rs"),
        ),
    ]
}

#[test]
fn inherited_elixir_alias_roots_fence_nested_alias_expansion() {
    let facts = generation(&[
        (
            "first.ex",
            "defmodule First.Tools do\n def helper(), do: :first\nend\n",
        ),
        ("a.ex", "defmodule A.Tools do\n def helper(), do: :a\nend\n"),
        (
            "caller.ex",
            "defmodule Caller do\n alias First, as: A\n def use_it do\n alias A.Tools, as: T\n T.helper()\n end\nend\n",
        ),
    ]);
    unresolved(&facts, "caller.ex", "helper");
}

#[test]
fn shell_loaded_scripts_that_change_directory_withdraw_source_proof() {
    for setup in ["cd sub\n", "source ./nested.sh\n"] {
        let facts = generation(&[
            (
                "main.sh",
                "source ./setup.sh\nsource ./tools.sh\nuse_it() { helper; }\n",
            ),
            ("setup.sh", setup),
            ("nested.sh", "cd sub\n"),
            ("tools.sh", "helper() { echo root; }\n"),
            ("sub/tools.sh", "helper() { echo sub; }\n"),
        ]);
        unresolved(&facts, "main.sh", "helper");
    }
}

#[test]
fn rust_orphan_nested_libraries_never_create_a_declared_cargo_target() {
    let fixtures = [
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        ),
        ("src/lib.rs", "fn unrelated() {}\n"),
        (
            "orphan/src/lib.rs",
            "pub mod helpers; fn use_it() { crate::helpers::helper(); }\n",
        ),
        ("orphan/src/helpers.rs", "pub fn helper() {}\n"),
    ];
    let index = fixture_index(&fixtures);
    let file = index
        .modules
        .exact
        .get("orphan/src/lib.rs")
        .and_then(|files| files.first())
        .unwrap_or_else(|| panic!("missing orphan file"));
    assert!(super::super::rust_root_ownership::root(&index, file).is_none());
    let facts = generation(&fixtures);
    // The existing base resolver has its own path behavior; the owned path adds no proof.
    let reference = call(&facts, "orphan/src/lib.rs", "crate::helpers::helper");
    assert_eq!(
        reference.resolution_provenance,
        "native-rust-qualified-path"
    );
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, "orphan/src/helpers.rs", "helper").symbol_id)
    );
}

fn fixture_extraction(
    input: (&str, &str),
    limits: SourceLimits,
) -> cartograph_extract::ExtractedFile {
    let (path, source) = input;
    let snapshot = cartograph_extract::SourceSnapshot::from_bytes_for_capability_validation(
        path,
        source.as_bytes(),
        limits,
    )
    .unwrap_or_else(|error| panic!("snapshot {path}: {error}"));
    NativeExtractor::new_for_capability_validation(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .unwrap_or_else(|error| panic!("extract {path}: {error}"))
}

fn facade_source(count: usize) -> String {
    let mut source = String::from("pub mod helpers;\n");
    for i in 0..count {
        writeln!(source, "pub use crate::helpers::helper as H_{i};")
            .unwrap_or_else(|error| panic!("facade export: {error}"));
    }
    source.push_str("fn use_it() {\n");
    for i in 0..count {
        writeln!(source, "crate::H_{i}();").unwrap_or_else(|error| panic!("facade call: {error}"));
    }
    source.push_str("}\n");
    source
}

#[test]
fn dynamic_source_loads_withdraw_direct_and_transitive_shell_proof() {
    for (caller, setup) in [
        (
            "source ./tools.sh\nsource \"$OTHER\"\nuse_it() { helper; }\n",
            "",
        ),
        (
            "source ./tools.sh\nsource ./setup.sh\nuse_it() { helper; }\n",
            "source \"$OTHER\"\n",
        ),
        (
            "source ./tools.sh\nsetup() { source \"$OTHER\"; }\nuse_it() { helper; }\n",
            "",
        ),
    ] {
        let facts = generation(&[
            ("main.sh", caller),
            ("tools.sh", "helper() { :; }\n"),
            ("setup.sh", setup),
        ]);
        unresolved(&facts, "main.sh", "helper");
    }
    let facts = generation(&[
        (
            "main.ps1",
            ". ./tools.ps1\n. $Other\nfunction use_it { helper }\n",
        ),
        ("tools.ps1", "function helper {}\n"),
    ]);
    unresolved(&facts, "main.ps1", "helper");
}
