use super::super::*;
use std::collections::{HashMap, HashSet};

const MAX_CYCLOMATIC_EXCLUSIVE: u16 = 15;
const MAX_FUNCTION_LINES_EXCLUSIVE: u32 = 100;
const MAX_PARAMETER_COUNT: u16 = 3;
const MAX_DISTINCT_CALLS_EXCLUSIVE: usize = 25;

#[test]
fn repr_modules_satisfy_the_source_health_limits() {
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("source limits: {error}"));
    let mut issues = Vec::new();
    for (path, source) in modules() {
        let snapshot = cartograph_extract::SourceSnapshot::from_bytes_for_capability_validation(
            path,
            source.as_bytes(),
            limits,
        )
        .unwrap_or_else(|error| panic!("snapshot {path}: {error}"));
        let file = NativeExtractor::new_for_capability_validation(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|error| panic!("extract {path}: {error}"));
        assert!(
            !file.symbols.is_empty(),
            "{path} has no extracted declarations"
        );
        issues.extend(check_limits(path, &file));
    }
    assert!(issues.is_empty(), "{}", issues.join("\n"));
}

fn check_limits(path: &str, file: &cartograph_extract::ExtractedFile) -> Vec<String> {
    let mut callees: HashMap<&SymbolId, HashSet<&str>> = HashMap::new();
    for reference in &file.references {
        if reference.kind == ReferenceKind::Calls
            && let Some(owner) = &reference.owner
        {
            callees
                .entry(owner)
                .or_default()
                .insert(reference.name.as_str());
        }
    }
    let mut issues = Vec::new();
    for symbol in &file.symbols {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        let lines = symbol.span.end_line() - symbol.span.start_line() + 1;
        let calls = callees.get(&symbol.id).map_or(0, HashSet::len);
        if symbol.health.cyclomatic >= MAX_CYCLOMATIC_EXCLUSIVE
            || lines >= MAX_FUNCTION_LINES_EXCLUSIVE
            || symbol.health.parameter_count > MAX_PARAMETER_COUNT
            || calls >= MAX_DISTINCT_CALLS_EXCLUSIVE
        {
            issues.push(format!(
                "{path}:{} {} cc={} params={} lines={lines} calls={calls}",
                symbol.span.start_line(),
                symbol.qualified_name,
                symbol.health.cyclomatic,
                symbol.health.parameter_count,
            ));
        }
    }
    issues
}

fn modules() -> [(&'static str, &'static str); 10] {
    [
        (
            "recursion.rs",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../cartograph-extract/src/walk/lisp_family/clojure/recursion.rs"
            )),
        ),
        ("java.rs", include_str!("../../enum_resolution/java.rs")),
        (
            "javascript_alias_exports.rs",
            include_str!("../../javascript_alias_exports.rs"),
        ),
        (
            "alias_syntax.rs",
            include_str!("../../javascript_alias_exports/syntax.rs"),
        ),
        (
            "python_class_members.rs",
            include_str!("../../python_class_members.rs"),
        ),
        (
            "python_classes.rs",
            include_str!("../../python_class_members/classes.rs"),
        ),
        (
            "python_syntax.rs",
            include_str!("../../python_class_members/syntax.rs"),
        ),
        (
            "repr_file_imports.rs",
            include_str!("../../repr_file_imports.rs"),
        ),
        (
            "reference_dispatch.rs",
            include_str!("../../reference_dispatch.rs"),
        ),
        shared_dispatch_source(),
    ]
}

fn shared_dispatch_source() -> (&'static str, &'static str) {
    const SIGNATURE: &str = "fn resolve_reference_body<Cancel>(";
    const NEXT_FUNCTION: &str = "\n/// Try module and import bindings";
    let source = include_str!("../../../native_pipeline.rs");
    let start = source
        .find(SIGNATURE)
        .unwrap_or_else(|| panic!("shared dispatch signature changed"));
    let function = source
        .get(start..)
        .and_then(|tail| tail.split_once(NEXT_FUNCTION))
        .map_or_else(
            || panic!("shared dispatch boundary changed"),
            |(function, _)| function,
        );
    ("native_pipeline.rs", function)
}
