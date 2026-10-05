//! Resolution of the Python, Go, and Rust facts restored for v1 parity:
//! package constants, embedded and instantiated types, decorators, module
//! variables, and supertraits resolve to their exact declaration, a qualified
//! path never falls back to a same-named local declaration, and the outcome
//! does not depend on input order.

use std::{fmt::Write as _, fs};

use tempfile::tempdir;

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, DRIFT_SCOPE_TASKS, Instant, ReferenceInput,
    ReferenceKind, SERIAL_WORKERS, SourceRoot, SymbolKind, TEST_SCOPE_BYTES, TEST_TIMEOUT,
    build_capability_generation, build_native_generation, capability_symbol, config,
};
use crate::stage::test_stage_runner;

#[test]
fn go_constant_reads_embedding_and_literals_resolve_within_their_package() {
    for reverse in [false, true] {
        let generation = build_capability_generation(
            &[
                ("api/limits.go", "package api\n\nconst MaxN = 10\n"),
                ("api/base.go", "package api\n\ntype Base struct{}\n"),
                (
                    "api/model.go",
                    "package api\n\nimport \"example.com/store\"\n\ntype Model struct {\n    Base\n    db *store.Base\n    X, Y Point\n}\n\nfunc New() Model {\n    _ = MaxN\n    return Model{}\n}\n",
                ),
                ("api/point.go", "package api\n\ntype Point struct{}\n"),
                ("other/limits.go", "package other\n\nconst MaxN = 20\n"),
            ],
            reverse,
        );
        let max = capability_symbol(&generation, "api/limits.go", "MaxN");
        let base = capability_symbol(&generation, "api/base.go", "Base");
        let point = capability_symbol(&generation, "api/point.go", "Point");
        let model = capability_symbol(&generation, "api/model.go", "Model");
        let new = capability_symbol(&generation, "api/model.go", "New");

        let read = CapabilityReferenceQuery::new(&generation, new)
            .named("MaxN", ReferenceKind::References);
        assert_eq!(
            read.target_symbol_id.as_ref(),
            Some(&max.symbol_id),
            "{reverse}"
        );
        let embedded =
            CapabilityReferenceQuery::new(&generation, model).named("Base", ReferenceKind::Extends);
        assert_eq!(
            embedded.target_symbol_id.as_ref(),
            Some(&base.symbol_id),
            "{reverse}"
        );
        let foreign =
            CapabilityReferenceQuery::new(&generation, model).named("Base", ReferenceKind::TypeOf);
        assert!(
            foreign.target_symbol_id.is_none(),
            "store.Base claimed the local Base: {reverse}"
        );
        let field_type =
            CapabilityReferenceQuery::new(&generation, model).named("Point", ReferenceKind::TypeOf);
        assert_eq!(field_type.target_symbol_id.as_ref(), Some(&point.symbol_id));
        assert_eq!(field_type.site_count, 1, "X, Y Point is one written type");
        let created = CapabilityReferenceQuery::new(&generation, new)
            .named("Model", ReferenceKind::Instantiates);
        assert_eq!(created.target_symbol_id.as_ref(), Some(&model.symbol_id));
    }
}

#[test]
fn cgo_calls_never_resolve_to_a_same_named_go_function() {
    let generation = build_capability_generation(
        &[(
            "native/wrap.go",
            "package native\n\nimport \"C\"\n\nfunc puts(text string) {}\n\nfunc Print(text string) {\n    C.puts(nil)\n}\n",
        )],
        false,
    );
    let print = capability_symbol(&generation, "native/wrap.go", "Print");
    let call =
        CapabilityReferenceQuery::new(&generation, print).named("puts", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none());
}

#[test]
fn python_decorators_and_module_variables_resolve_through_imports() {
    for reverse in [false, true] {
        let generation = build_capability_generation(
            &[
                (
                    "app/decorators.py",
                    "def trace(function):\n    return function\n",
                ),
                ("app/settings.py", "router = make_router()\n"),
                (
                    "app/views.py",
                    "from .decorators import trace\nfrom .settings import router\n\n@trace\ndef handler():\n    pass\n",
                ),
            ],
            reverse,
        );
        let trace = capability_symbol(&generation, "app/decorators.py", "trace");
        let router = capability_symbol(&generation, "app/settings.py", "router");
        let handler = capability_symbol(&generation, "app/views.py", "handler");
        assert_eq!(router.symbol_kind, SymbolKind::Variable.as_str());

        let decorates = CapabilityReferenceQuery::new(&generation, handler)
            .named("trace", ReferenceKind::Decorates);
        assert_eq!(decorates.target_symbol_id.as_ref(), Some(&trace.symbol_id));
        let imported = file_reference(&generation, "app/views.py", "router");
        assert_eq!(imported.target_symbol_id.as_ref(), Some(&router.symbol_id));
    }
}

#[test]
fn rust_supertraits_and_struct_expressions_resolve_to_their_declarations() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "pub trait Base {}\npub trait Worker: Base + Send {}\npub struct Config { limit: usize }\npub const LIMIT: usize = 3;\npub fn build() -> Config {\n    Config { limit: LIMIT }\n}\n",
        )],
        false,
    );
    let base = capability_symbol(&generation, "src/lib.rs", "Base");
    let worker = capability_symbol(&generation, "src/lib.rs", "Worker");
    let config = capability_symbol(&generation, "src/lib.rs", "Config");
    let limit = capability_symbol(&generation, "src/lib.rs", "LIMIT");
    let build = capability_symbol(&generation, "src/lib.rs", "build");

    let supertrait =
        CapabilityReferenceQuery::new(&generation, worker).named("Base", ReferenceKind::Extends);
    assert_eq!(supertrait.target_symbol_id.as_ref(), Some(&base.symbol_id));
    let created = CapabilityReferenceQuery::new(&generation, build)
        .named("Config", ReferenceKind::Instantiates);
    assert_eq!(created.target_symbol_id.as_ref(), Some(&config.symbol_id));
    let read =
        CapabilityReferenceQuery::new(&generation, build).named("LIMIT", ReferenceKind::References);
    assert_eq!(read.target_symbol_id.as_ref(), Some(&limit.symbol_id));
}

/// Names in a generated `iota` block dense enough that one symbol per name
/// exceeds the per-file extraction output limit.
const DENSE_IOTA_NAMES: usize = 3_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dense_generated_go_constants_still_build_a_generation() {
    let mut source = String::from("package ops\n\ntype Op int\n\nconst (\n\tOp0 Op = iota\n");
    for index in 1..DENSE_IOTA_NAMES {
        assert!(writeln!(&mut source, "\tOp{index}").is_ok());
    }
    source.push_str(")\n\nfunc helper() {}\n\nfunc Use() { helper() }\n");
    let directory =
        tempdir().unwrap_or_else(|error| panic!("could not create dense fixture: {error}"));
    assert!(fs::write(directory.path().join("ops.go"), &source).is_ok());
    let source_root = SourceRoot::open(directory.path())
        .unwrap_or_else(|error| panic!("could not open dense fixture: {error}"));
    let (runner, tasks, cancellation) =
        test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;

    let generation = build_native_generation(&runner, source_root, config(SERIAL_WORKERS))
        .await
        .unwrap_or_else(|error| panic!("a dense generated file failed the generation: {error}"));

    let report = generation.report();
    assert_eq!(report.diagnostics(), 1, "the omission is reported");
    assert_eq!(report.degraded_files().len(), 0);
    let facts = generation.facts();
    assert!(
        facts
            .symbols()
            .iter()
            .all(|symbol| symbol.symbol_kind != SymbolKind::Constant.as_str())
    );
    let helper = capability_symbol(facts, "ops.go", "helper");
    let caller = capability_symbol(facts, "ops.go", "Use");
    let call = CapabilityReferenceQuery::new(facts, caller).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&helper.symbol_id));
    drop(cancellation);
    let task_report = tasks
        .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
        .await;
    assert!(task_report.all_joined);
    assert!(!task_report.worker_failed);
}

/// The file-level reference an import declaration records for `name`.
fn file_reference<'facts>(
    facts: &'facts CanonicalGenerationFacts,
    path: &str,
    name: &str,
) -> &'facts ReferenceInput {
    let file = facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"));
    let file_symbol = facts
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.file_id == file.file_id && symbol.symbol_kind == SymbolKind::File.as_str()
        })
        .unwrap_or_else(|| panic!("missing file symbol {path}"));
    CapabilityReferenceQuery::new(facts, file_symbol).named(name, ReferenceKind::References)
}
