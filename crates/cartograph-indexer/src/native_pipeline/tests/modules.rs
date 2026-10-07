use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceKind, build_capability_generation,
    capability_symbol,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reverse = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reverse.digest());
    assert_eq!(forward.references(), reverse.references());
    forward
}

fn ocaml_base(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    super::generic_repair::build_generation(
        super::generic_repair::CapabilityGenerationRequest {
            fixtures,
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: super::TEST_GENERATION_BYTES,
        },
        |file| {
            file.import_bindings
                .retain(|binding| !binding.module_specifier.starts_with("<ocaml-"));
        },
        || false,
    )
}

fn assert_target(
    facts: &CanonicalGenerationFacts,
    source: (&str, &str, &str),
    target: (&str, &str, &str),
) {
    let caller = capability_symbol(facts, source.0, source.1);
    let reference =
        CapabilityReferenceQuery::new(facts, caller).named(source.2, ReferenceKind::Calls);
    let symbol = capability_symbol(facts, target.0, target.1);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&symbol.symbol_id));
    assert_eq!(reference.resolution_provenance, target.2);
}

fn assert_kind_target(
    facts: &CanonicalGenerationFacts,
    source: (&str, &str, &str, ReferenceKind),
    target: (&str, &str, &str),
) {
    let caller = capability_symbol(facts, source.0, source.1);
    let reference = CapabilityReferenceQuery::new(facts, caller).named(source.2, source.3);
    let symbol = capability_symbol(facts, target.0, target.1);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&symbol.symbol_id));
    assert_eq!(reference.resolution_provenance, target.2);
}

#[test]
fn go_module_imports_bind_the_package_and_not_a_same_named_directory() {
    let facts = generation(&[
        ("go.mod", "module example.com/shop\n\ngo 1.22\n"),
        (
            "cmd/main.go",
            "package main\nimport p \"example.com/shop/internal/tools\"\nfunc run() { p.Helper(); p.private(); }\n",
        ),
        ("internal/tools/a.go", "package tools\nfunc Other() {}\n"),
        (
            "internal/tools/b.go",
            "package tools\nfunc Helper() {}\nfunc private() {}\n",
        ),
        (
            "other/internal/tools/b.go",
            "package tools\nfunc Helper() {}\n",
        ),
    ]);
    assert_target(
        &facts,
        ("cmd/main.go", "run", "p.Helper"),
        ("internal/tools/b.go", "Helper", "native-go-module-import"),
    );
    let run = capability_symbol(&facts, "cmd/main.go", "run");
    assert!(
        CapabilityReferenceQuery::new(&facts, run)
            .named("p.private", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn go_module_mapping_preserves_an_existing_directory_rule_target_and_provenance() {
    let facts = generation(&[
        ("example.com/shop/go.mod", "module example.com/shop\n"),
        (
            "example.com/shop/main.go",
            "package main\nimport p \"example.com/shop/tools\"\nfunc run() { p.Helper() }\n",
        ),
        (
            "example.com/shop/tools/helper.go",
            "package tools\nfunc Helper() {}\n",
        ),
        ("other/tools/helper.go", "package tools\nfunc Helper() {}\n"),
    ]);
    assert_target(
        &facts,
        ("example.com/shop/main.go", "run", "p.Helper"),
        (
            "example.com/shop/tools/helper.go",
            "Helper",
            "native-go-import-path",
        ),
    );
}

#[test]
fn go_module_imports_abstain_for_an_external_prefix_or_nested_module() {
    let facts = generation(&[
        ("go.mod", "module example.com/shop\n"),
        (
            "main.go",
            "package main\nimport p \"example.com/other/internal/tools\"\nimport q \"example.com/shop/nested/tools\"\nfunc run() { p.Helper(); q.Helper(); }\n",
        ),
        ("internal/tools/a.go", "package tools\nfunc Helper() {}\n"),
        ("nested/go.mod", "module example.com/separate\n"),
        ("nested/tools/a.go", "package tools\nfunc Helper() {}\n"),
    ]);
    let run = capability_symbol(&facts, "main.go", "run");
    for name in ["p.Helper", "q.Helper"] {
        assert!(
            CapabilityReferenceQuery::new(&facts, run)
                .named(name, ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn go_module_imports_do_not_bind_test_packages_or_mixed_package_directories() {
    for extra in ["tools/helper_test.go", "tools/helper.go"] {
        let facts = generation(&[
            ("go.mod", "module example.com/shop\n"),
            (
                "main.go",
                "package main\nimport p \"example.com/shop/tools\"\nfunc run() { p.Helper() }\n",
            ),
            ("tools/a.go", "package tools\nfunc Other() {}\n"),
            (extra, "package tools_test\nfunc Helper() {}\n"),
        ]);
        let run = capability_symbol(&facts, "main.go", "run");
        let call =
            CapabilityReferenceQuery::new(&facts, run).named("p.Helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{extra}: {call:?}");
    }
}

#[test]
fn go_module_package_types_fields_and_typed_receiver_use_the_same_path_proof() {
    let facts = generation(&[
        ("go.mod", "module example.com/shop\n"),
        (
            "handler.go",
            "package shop\nimport m \"example.com/shop/models\"\nimport s \"example.com/shop/service\"\ntype Handler struct { svc *s.Service }\nfunc (h *Handler) Run() m.User { h.svc.Register(); region := m.DefaultRegion; _ = region; return m.User{} }\n",
        ),
        (
            "models/user.go",
            "package models\ntype User struct {}\nvar DefaultRegion = 1\n",
        ),
        (
            "service/user.go",
            "package service\ntype Service struct {}\nfunc (s *Service) Register() {}\n",
        ),
    ]);
    assert_target(
        &facts,
        ("handler.go", "Handler::Run", "h.svc.Register"),
        (
            "service/user.go",
            "Service::Register",
            "native-explicit-receiver-type",
        ),
    );
    assert_kind_target(
        &facts,
        ("handler.go", "Handler::Run", "User", ReferenceKind::Returns),
        ("models/user.go", "User", "native-go-module-import"),
    );
    assert_kind_target(
        &facts,
        (
            "handler.go",
            "Handler::Run",
            "DefaultRegion",
            ReferenceKind::FieldAccess,
        ),
        ("models/user.go", "DefaultRegion", "native-go-module-import"),
    );
}

#[test]
fn go_module_evidence_abstains_when_an_import_alias_is_shadowed() {
    let facts = generation(&[
        ("go.mod", "module example.com/shop\n"),
        (
            "main.go",
            "package main\nimport m \"example.com/shop/models\"\nfunc run(m interface{}) { m.Helper(); value := m.DefaultRegion; _ = value }\n",
        ),
        (
            "models/user.go",
            "package models\nfunc Helper() {}\nvar DefaultRegion = 1\n",
        ),
    ]);
    let run = capability_symbol(&facts, "main.go", "run");
    for (name, kind) in [
        ("m.Helper", ReferenceKind::Calls),
        ("DefaultRegion", ReferenceKind::FieldAccess),
    ] {
        let reference = CapabilityReferenceQuery::new(&facts, run).named(name, kind);
        assert_ne!(reference.resolution_provenance, "native-go-module-import");
        assert!(reference.target_symbol_id.is_none());
    }
}

#[test]
fn go_manifest_ignores_dependency_lines_and_rejects_missing_or_duplicate_modules() {
    for manifest in [
        "require (\n module v1.2.3\n)\n",
        "module v1.2.3\nmodule example.com/other\n",
        "module v1.2.3\nrequire (\n",
    ] {
        let facts = generation(&[
            ("go.mod", manifest),
            (
                "main.go",
                "package main\nimport p \"v1.2.3/tools\"\nfunc run() { p.Helper() }\n",
            ),
            ("tools/helper.go", "package tools\nfunc Helper() {}\n"),
        ]);
        let run = capability_symbol(&facts, "main.go", "run");
        assert!(
            CapabilityReferenceQuery::new(&facts, run)
                .named("p.Helper", ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
    let facts = generation(&[
        (
            "go.mod",
            "module example.com/shop\nrequire (\n module v1.2.3\n)\n",
        ),
        (
            "main.go",
            "package main\nimport p \"example.com/shop/tools\"\nfunc run() { p.Helper() }\n",
        ),
        ("tools/helper.go", "package tools\nfunc Helper() {}\n"),
    ]);
    assert_target(
        &facts,
        ("main.go", "run", "p.Helper"),
        ("tools/helper.go", "Helper", "native-go-module-import"),
    );
}

#[test]
fn ocaml_compilation_units_and_open_bind_exact_module_members() {
    let facts = generation(&[
        (
            "app/main.ml",
            "open Shapes\nlet run () = Registry.Registry.register (); Registry.make_counter (); print_all ()\n",
        ),
        (
            "lib/registry.ml",
            "module Registry = struct\n let register () = ()\n let lookup () = ()\nend\nlet make_counter () = ()\nlet require () = Registry.lookup ()\n",
        ),
        ("lib/shapes.ml", "let print_all () = ()\n"),
        ("other.ml", "let print_all () = ()\n"),
    ]);
    for (name, file, target) in [
        ("register", "lib/registry.ml", "Registry.register"),
        ("make_counter", "lib/registry.ml", "make_counter"),
        ("print_all", "lib/shapes.ml", "print_all"),
    ] {
        assert_target(
            &facts,
            ("app/main.ml", "run", name),
            (file, target, "native-ocaml-module"),
        );
    }
    assert_target(
        &facts,
        ("lib/registry.ml", "require", "lookup"),
        ("lib/registry.ml", "Registry.lookup", "native-ocaml-module"),
    );
}

#[test]
fn ocaml_ambiguous_units_interfaces_and_module_aliases_withhold_added_proof() {
    let facts = generation(&[
        (
            "main.ml",
            "open Shapes\nmodule Alias = Shapes\nlet run () = print_all (); Alias.print_all (); Hidden.secret ()\n",
        ),
        ("a/shapes.ml", "let print_all () = ()\n"),
        ("b/shapes.ml", "let print_all () = ()\n"),
        ("hidden.ml", "let secret () = ()\n"),
        ("hidden.mli", "val visible : unit -> unit\n"),
    ]);
    let run = capability_symbol(&facts, "main.ml", "run");
    assert!(
        facts
            .references()
            .iter()
            .filter(
                |reference| reference.owner_symbol_id.as_ref() == Some(&run.symbol_id)
                    && reference.reference_kind == "calls"
            )
            .all(|reference| reference.target_symbol_id.is_none())
    );
}

#[test]
fn ocaml_open_shadowing_and_callable_parameters_withhold_added_proof() {
    let facts = generation(&[
        (
            "m.ml",
            "module X = struct let helper () = 1 end\nlet helper () = 1\n",
        ),
        ("x.ml", "let helper () = 2\n"),
        ("qualified.ml", "open M\nlet run () = X.helper ()\n"),
        ("parameter.ml", "open M\nlet run helper = helper ()\n"),
        (
            "unknown.ml",
            "open External\nopen M\nlet run () = helper ()\n",
        ),
    ]);
    for file in ["qualified.ml", "parameter.ml", "unknown.ml"] {
        let run = capability_symbol(&facts, file, "run");
        assert!(
            CapabilityReferenceQuery::new(&facts, run)
                .named("helper", ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn ocaml_local_functions_keep_their_existing_resolution_with_an_open() {
    let facts = generation(&[
        ("shapes.ml", "let helper () = 1\n"),
        (
            "main.ml",
            "open Shapes\nlet helper () = 2\nlet run () = helper ()\n",
        ),
    ]);
    assert_target(
        &facts,
        ("main.ml", "run", "helper"),
        ("main.ml", "helper", "native-exact-same-file"),
    );
}

#[test]
fn ocaml_nested_modules_preserve_base_resolution_instead_of_a_same_named_unit() {
    let fixtures = [
        (
            "main.ml",
            "module Local = struct\n module X = struct let helper () = 2 end\n let run () = X.helper ()\nend\n",
        ),
        ("x.ml", "let helper () = 1\n"),
    ];
    let facts = generation(&fixtures);
    let run = capability_symbol(&facts, "main.ml", "Local.run");
    let call = CapabilityReferenceQuery::new(&facts, run).named("helper", ReferenceKind::Calls);
    let foreign = capability_symbol(&facts, "x.ml", "helper");
    assert_ne!(call.target_symbol_id.as_ref(), Some(&foreign.symbol_id));
    let base = ocaml_base(&fixtures);
    super::generic_repair::assert_base_reference(&base, call);
}

#[test]
fn ocaml_nonroot_module_bindings_never_select_an_unrelated_compilation_unit() {
    let fixtures = [
        ("x.ml", "let helper () = 1\n"),
        (
            "main.ml",
            "let run () = let module X = struct let helper () = 2 end in X.helper ()\n",
        ),
        (
            "nested.ml",
            "module Local = struct module X = struct let helper () = 2 end let run () = X.helper () end\n",
        ),
        (
            "functor.ml",
            "module type S = sig val helper : unit -> int end\nmodule F (X : S) = struct let run () = X.helper () end\n",
        ),
        (
            "unpack.ml",
            "module type S = sig val helper : unit -> int end\nlet run packed = let module X = (val packed : S) in X.helper ()\n",
        ),
    ];
    let facts = generation(&fixtures);
    let base = ocaml_base(&fixtures);
    let foreign = capability_symbol(&facts, "x.ml", "helper");
    for (file, name) in [
        ("main.ml", "run"),
        ("nested.ml", "Local.run"),
        ("functor.ml", "F.run"),
        ("unpack.ml", "run"),
    ] {
        let caller = capability_symbol(&facts, file, name);
        let call =
            CapabilityReferenceQuery::new(&facts, caller).named("helper", ReferenceKind::Calls);
        assert_ne!(
            call.target_symbol_id.as_ref(),
            Some(&foreign.symbol_id),
            "{file}: {call:?}"
        );
        assert_ne!(
            call.resolution_provenance, "native-ocaml-module",
            "{file}: {call:?}"
        );
        super::generic_repair::assert_base_reference(&base, call);
    }
}

#[test]
fn ocaml_indexed_opens_preserve_shadow_and_ambiguity_activation_positions() {
    let facts = generation(&[
        ("x.ml", "let helper () = 1\nlet secret () = 1\n"),
        (
            "m.ml",
            "module X = struct let helper () = 2 end\nlet helper () = 2\n",
        ),
        ("n.ml", "let helper () = 3\n"),
        ("other.ml", "let secret () = 3\n"),
        (
            "main.ml",
            "let before () = X.helper ()\nopen M\nlet after () = X.helper ()\nlet unique () = helper ()\nopen N\nlet ambiguous () = helper ()\n",
        ),
        (
            "shadowed_open.ml",
            "open M\nopen X\nlet run () = secret ()\n",
        ),
    ]);
    assert_target(
        &facts,
        ("main.ml", "before", "helper"),
        ("x.ml", "helper", "native-ocaml-module"),
    );
    assert_target(
        &facts,
        ("main.ml", "unique", "helper"),
        ("m.ml", "helper", "native-ocaml-module"),
    );
    for name in ["after", "ambiguous"] {
        let caller = capability_symbol(&facts, "main.ml", name);
        let call =
            CapabilityReferenceQuery::new(&facts, caller).named("helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{name}: {call:?}");
    }
    let caller = capability_symbol(&facts, "shadowed_open.ml", "run");
    let call = CapabilityReferenceQuery::new(&facts, caller).named("secret", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

fn measured_ocaml_opens(count: usize) -> (CanonicalGenerationFacts, usize) {
    let mut fixtures = vec![("x.ml".to_owned(), "let helper () = 1\n".to_owned())];
    let mut source = String::new();
    for ordinal in 0..count {
        fixtures.push((
            format!("m{ordinal}.ml"),
            format!("let marker () = {ordinal}\n"),
        ));
        super::append_fixture_text(&mut source, format_args!("open M{ordinal}\n"));
    }
    source.push_str("let run () =\n");
    for _ in 0..count {
        source.push_str(" X.helper ();\n");
    }
    source.push_str(" ()\n");
    fixtures.push(("main.ml".to_owned(), source));
    let fixtures = fixtures
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let mut polls = 0_usize;
    let facts = super::generic_repair::build_generation(
        super::generic_repair::CapabilityGenerationRequest {
            fixtures: &fixtures,
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: super::TEST_GENERATION_BYTES,
        },
        |_| {},
        || {
            polls += 1;
            false
        },
    );
    (facts, polls)
}

#[test]
fn ocaml_untagged_opened_values_and_rebound_members_preserve_base_abstention() {
    let fixtures = [
        ("n.ml", "let helper () = 1\n"),
        (
            "m.ml",
            "let make_helper () = fun () -> 2\nlet helper = make_helper ()\n",
        ),
        (
            "rebound.ml",
            "let helper () = 1\nlet make_helper () = fun () -> 2\nlet helper = make_helper ()\n",
        ),
        (
            "main.ml",
            "open N\nlet before () = helper ()\nopen M\nlet run () = helper ()\n",
        ),
        ("qualified.ml", "let run () = Rebound.helper ()\n"),
        ("typed.ml", "let (helper : unit -> int) = fun () -> 2\n"),
        (
            "typed_open.ml",
            "open N\nopen Typed\nlet run () = helper ()\n",
        ),
        (
            "typed_source.ml",
            "open N\nlet (helper : unit -> int) = fun () -> 2\nlet run () = helper ()\n",
        ),
        (
            "typed_rebound.ml",
            "let helper () = 1\nlet (helper : unit -> int) = fun () -> 2\n",
        ),
        (
            "typed_qualified.ml",
            "let run () = Typed_rebound.helper ()\n",
        ),
    ];
    let facts = generation(&fixtures);
    let base = ocaml_base(&fixtures);
    assert_target(
        &facts,
        ("main.ml", "before", "helper"),
        ("n.ml", "helper", "native-ocaml-module"),
    );
    for file in [
        "main.ml",
        "qualified.ml",
        "typed_open.ml",
        "typed_source.ml",
        "typed_qualified.ml",
    ] {
        let caller = capability_symbol(&facts, file, "run");
        let call =
            CapabilityReferenceQuery::new(&facts, caller).named("helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{file}: {call:?}");
        assert_ne!(call.resolution_provenance, "native-ocaml-module");
        super::generic_repair::assert_base_reference(&base, call);
    }
}

#[test]
fn ocaml_root_opens_and_qualified_calls_have_indexed_resolution_work() {
    const SMALL_OPEN_COUNT: usize = 256;
    const SCALE_FACTOR: usize = 2;
    const MAX_POLLS_PER_OPEN: usize = 512;
    const LINEAR_NUMERATOR: usize = 21;
    const LINEAR_DENOMINATOR: usize = 10;
    let mut samples = [0_usize; 2];
    for (slot, count) in [SMALL_OPEN_COUNT, SMALL_OPEN_COUNT * SCALE_FACTOR]
        .into_iter()
        .enumerate()
    {
        let (facts, polls) = measured_ocaml_opens(count);
        let caller = capability_symbol(&facts, "main.ml", "run");
        let target = capability_symbol(&facts, "x.ml", "helper");
        let calls = facts
            .references()
            .iter()
            .filter(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&caller.symbol_id)
                    && reference.reference_kind == "calls"
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), count);
        assert!(calls.iter().all(
            |call| call.target_symbol_id.as_ref() == Some(&target.symbol_id)
                && call.resolution_provenance == "native-ocaml-module"
        ));
        assert!(
            polls < count * MAX_POLLS_PER_OPEN,
            "{count} opens: {polls} cancellation polls"
        );
        samples[slot] = polls;
    }
    assert!(
        samples[1] * LINEAR_DENOMINATOR <= samples[0] * LINEAR_NUMERATOR,
        "nonlinear open work: {samples:?}"
    );
    eprintln!("OCaml indexed-open cancellation polls: {samples:?}");
}

#[test]
fn ocaml_module_parameters_object_sends_and_value_aliases_withhold_added_proof() {
    let facts = generation(&[
        ("x.ml", "let helper () = 1\n"),
        ("m.ml", "let helper () = 1\n"),
        (
            "module_parameter.ml",
            "module type S = sig val helper : unit -> int end\nlet run (module X : S) = X.helper ()\n",
        ),
        ("object_send.ml", "open M\nlet run o = o#helper ()\n"),
        (
            "value_alias.ml",
            "open M\nlet helper = make_helper ()\nlet run () = helper ()\n",
        ),
    ]);
    for file in ["module_parameter.ml", "object_send.ml", "value_alias.ml"] {
        let run = capability_symbol(&facts, file, "run");
        let call = CapabilityReferenceQuery::new(&facts, run).named("helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{file}: {call:?}");
    }
}

#[test]
fn julia_inclusion_and_overloads_do_not_prove_an_unqualified_target() {
    let facts = generation(&[
        (
            "src/shapes.jl",
            "module Shapes\nabstract type Shape end\nstruct Circle <: Shape end\nstruct Rect <: Shape end\narea(c::Circle) = 1\narea(r::Rect) = 2\nfunction describe(s::Shape)\n area(s)\nend\nend\n",
        ),
        (
            "src/report.jl",
            "module Report\nfunction summarize(shapes)\n length(shapes)\nend\nend\n",
        ),
        (
            "test/check.jl",
            "include(\"../src/report.jl\")\nfunction run_checks()\n summarize([])\nend\n",
        ),
    ]);
    for (file, owner, name) in [
        ("src/shapes.jl", "Shapes.describe", "area"),
        ("test/check.jl", "run_checks", "summarize"),
    ] {
        let caller = capability_symbol(&facts, file, owner);
        assert!(
            CapabilityReferenceQuery::new(&facts, caller)
                .named(name, ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn pascal_paired_routine_targets_the_implementation_in_its_own_unit() {
    let facts = generation(&[
        (
            "auth.pas",
            "unit Auth;\ninterface\nprocedure DoHelper(N: Integer);\nprocedure Run;\nimplementation\nprocedure DoHelper(N: Integer); begin end;\nprocedure Run; begin DoHelper(1); end;\nend.\n",
        ),
        (
            "other.pas",
            "unit Other; interface procedure DoHelper(N: Integer); implementation procedure DoHelper(N: Integer); begin end; end.\n",
        ),
    ]);
    let helper = capability_symbol(&facts, "auth.pas", "DoHelper");
    assert_eq!(helper.start_line, 6);
    assert_target(
        &facts,
        ("auth.pas", "Run", "DoHelper"),
        ("auth.pas", "DoHelper", "native-exact-same-file"),
    );
}
