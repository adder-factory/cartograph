//! Bounds on the optional Python, Go, and Rust enrichment facts: dense
//! generated files keep their core facts instead of failing extraction, and
//! parameter shadowing stays bounded per scope however large the scope.

mod dependency_ownership;

use std::fmt::Write as _;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{
    DiagnosticCode, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT_BYTES: usize = 1024 * 1024;
/// The empty name list, for asserting that nothing was recorded.
const NONE: [&str; 0] = [];
/// The empty diagnostic list of a fully extracted file.
const NO_DIAGNOSTICS: [DiagnosticCode; 0] = [];
/// Names in a generated table dense enough that one fact per name exceeds the
/// per-file output limit.
const DENSE_TABLE_NAMES: usize = 3_000;
/// Elements of a single-line generated literal table dense enough that one
/// fact per element exceeds the per-file output limit.
const DENSE_LITERAL_ELEMENTS: usize = 20_000;
/// Reads in a generated constant-read table dense enough to exceed the limit.
const DENSE_READ_LINES: usize = 4_500;
/// Names in an ordinary block, well within the per-file output limit.
const ORDINARY_TABLE_NAMES: usize = 200;
/// Parameters in a scope beyond the per-scope binding scan budget.
const SATURATING_PARAMETERS: usize = 4_200;
/// Calls in a file whose core facts fill its output limit almost exactly.
const FILLING_CALLS: usize = 4_500;
/// Comment padding that leaves the filling file's core facts no room for
/// another fact once its optional facts are omitted.
const FILLING_PADDING_BYTES: usize = 9_225;
/// Names of a multi-name Go declaration whose facts fit the output limit.
const MULTI_NAME_FIELDS: usize = 200;
/// Names of a long multi-name Go constant declaration in a body.
const MULTI_NAME_CONSTANTS: usize = 4_000;

#[test]
fn dense_go_constant_blocks_keep_core_facts_instead_of_failing() {
    let mut source = String::from("package ops\n\ntype Op int\n\nconst (\n\tOp0 Op = iota\n");
    for index in 1..DENSE_TABLE_NAMES {
        push_line(&mut source, &format!("\tOp{index}"));
    }
    source.push_str(")\n\nfunc Use() { helper() }\n");

    let file = extract("ops/ops.go", &source);

    assert_optional_facts_omitted(&file);
    assert_eq!(names_of_kind(&file, SymbolKind::Constant), NONE);
    assert_eq!(names_of_kind(&file, SymbolKind::Function), ["Use"]);
    assert!(has_reference(&file, "helper", ReferenceKind::Calls));
}

#[test]
fn dense_go_literal_tables_keep_core_facts_instead_of_failing() {
    for element in ["Limit,", "T{},"] {
        let source = format!(
            "package ops\n\nvar table = []Op{{{}}}\n\nfunc Use() {{ helper() }}\n",
            element.repeat(DENSE_LITERAL_ELEMENTS)
        );

        let file = extract("ops/table.go", &source);

        assert_optional_facts_omitted(&file);
        assert_eq!(names_of_kind(&file, SymbolKind::Variable), NONE);
        assert!(!has_reference(&file, "Limit", ReferenceKind::References));
        assert!(!has_reference(&file, "T", ReferenceKind::Instantiates));
        assert!(has_reference(&file, "helper", ReferenceKind::Calls));
    }
}

#[test]
fn optional_facts_that_leave_no_room_for_framework_facts_are_omitted() {
    // Each line's package variable and its instantiation fit the walk's
    // output limit, but the framework stage's command route, which shares
    // that limit, then exceeds it; without the optional facts it fits.
    let mut source = String::from("package cmds\n\nimport \"github.com/spf13/cobra\"\n\n");
    for index in 0..DENSE_TABLE_NAMES {
        push_line(
            &mut source,
            &format!("var Cmd{index} = &cobra.Command{{Use: \"c{index}\"}}"),
        );
    }

    let file = extract("cmds/cmds.go", &source);

    assert_optional_facts_omitted(&file);
    assert_eq!(names_of_kind(&file, SymbolKind::Variable), NONE);
    assert!(!has_reference(
        &file,
        "Command",
        ReferenceKind::Instantiates
    ));
    assert_eq!(
        names_of_kind(&file, SymbolKind::Route).len(),
        DENSE_TABLE_NAMES
    );
}

#[test]
fn a_fallback_without_room_for_its_diagnostic_keeps_its_core_facts() {
    // The core facts alone fit, and the constant read pushes the first pass
    // over the limit; the fallback must not then fail on its own diagnostic.
    let source = format!(
        "fn f(){{{}let _=LIMIT;}}\n/*{}*/\n",
        "a();".repeat(FILLING_CALLS),
        " ".repeat(FILLING_PADDING_BYTES)
    );

    let file = extract("src/a.rs", &source);

    assert!(
        diagnostic_codes(&file)
            .iter()
            .all(|code| *code == DiagnosticCode::OptionalFactsOmitted)
    );
    assert!(!has_reference(&file, "LIMIT", ReferenceKind::References));
    assert_eq!(
        file.references
            .iter()
            .filter(|reference| reference.name == "a" && reference.kind == ReferenceKind::Calls)
            .count(),
        FILLING_CALLS
    );
}

#[test]
fn multi_name_go_declarations_declare_and_bind_every_name() {
    let fields = (0..MULTI_NAME_FIELDS)
        .map(|index| format!("F{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let constants = (0..MULTI_NAME_CONSTANTS)
        .map(|index| format!("K{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = vec!["1"; MULTI_NAME_CONSTANTS].join(", ");
    let source = format!(
        "package p\n\ntype S struct {{ {fields} int }}\n\nfunc f(P0, P1 int) int {{\n\tconst {constants} = {values}\n\treturn P0 + Limit\n}}\n"
    );

    let file = extract("p/multi.go", &source);

    assert_eq!(diagnostic_codes(&file), NO_DIAGNOSTICS);
    assert_eq!(
        names_of_kind(&file, SymbolKind::Field).len(),
        MULTI_NAME_FIELDS
    );
    assert!(has_reference(&file, "Limit", ReferenceKind::References));
    assert!(!file.references.iter().any(|reference| {
        reference.kind == ReferenceKind::References
            && (reference.name.starts_with('K') || reference.name.starts_with('P'))
    }));
}

#[test]
fn dense_rust_constant_reads_keep_core_facts_instead_of_failing() {
    let mut source = String::from("fn table() {\n    helper();\n");
    for _ in 0..DENSE_READ_LINES {
        push_line(&mut source, "    let _ = LIMIT_X + LIMIT_Y;");
    }
    source.push_str("}\n");

    let file = extract("src/table.rs", &source);

    assert_optional_facts_omitted(&file);
    assert!(!has_reference(&file, "LIMIT_X", ReferenceKind::References));
    assert_eq!(names_of_kind(&file, SymbolKind::Function), ["table"]);
    assert!(has_reference(&file, "helper", ReferenceKind::Calls));
}

#[test]
fn ordinary_tables_keep_their_optional_facts_without_a_diagnostic() {
    let mut go = String::from("package ops\n\ntype Op int\n\nconst (\n\tOp0 Op = iota\n");
    for index in 1..ORDINARY_TABLE_NAMES {
        push_line(&mut go, &format!("\tOp{index}"));
    }
    go.push_str(")\n\nfunc Use() Op { return Op7 }\n");
    let mut rust = String::from("fn table() {\n");
    for _ in 0..ORDINARY_TABLE_NAMES {
        push_line(&mut rust, "    let _ = LIMIT_X + LIMIT_Y;");
    }
    rust.push_str("}\n");

    let go = extract("ops/ops.go", &go);
    let rust = extract("src/table.rs", &rust);

    assert_eq!(diagnostic_codes(&go), NO_DIAGNOSTICS);
    assert_eq!(
        names_of_kind(&go, SymbolKind::Constant).len(),
        ORDINARY_TABLE_NAMES
    );
    assert!(has_reference(&go, "Op7", ReferenceKind::References));
    assert_eq!(diagnostic_codes(&rust), NO_DIAGNOSTICS);
    assert!(has_reference(&rust, "LIMIT_X", ReferenceKind::References));
}

#[test]
fn rust_scopes_beyond_the_binding_scan_budget_bind_every_name() {
    let mut source = String::from("fn wide<");
    for index in 0..SATURATING_PARAMETERS {
        write_or_panic(&mut source, &format!("const P{index}: usize, "));
    }
    source.push_str(">() -> usize { LIMIT_WIDE }\n");
    source.push_str("fn narrow<const N: usize>() -> usize { LIMIT_NARROW + N }\n");
    source.push_str("fn shadowed<const LIMIT_SHADOWED: usize>() -> usize { LIMIT_SHADOWED }\n");

    let file = extract("src/wide.rs", &source);

    assert_eq!(diagnostic_codes(&file), NO_DIAGNOSTICS);
    assert!(!has_reference(
        &file,
        "LIMIT_WIDE",
        ReferenceKind::References
    ));
    assert!(has_reference(
        &file,
        "LIMIT_NARROW",
        ReferenceKind::References
    ));
    assert!(!has_reference(
        &file,
        "LIMIT_SHADOWED",
        ReferenceKind::References
    ));
}

#[test]
fn rust_closure_shorthand_bindings_shadow_constants() {
    let file = extract(
        "src/closure.rs",
        "fn run() -> u8 {\n    let read = |S { LIMIT_FIELD }: S| LIMIT_FIELD;\n    read(S { LIMIT_FIELD: LIMIT_OUTER })\n}\n",
    );

    assert!(!has_reference(
        &file,
        "LIMIT_FIELD",
        ReferenceKind::References
    ));
    assert!(has_reference(
        &file,
        "LIMIT_OUTER",
        ReferenceKind::References
    ));
}

#[test]
fn go_scopes_beyond_the_binding_scan_budget_bind_every_name() {
    let mut source = String::from("package p\n\nfunc Wide(");
    for index in 0..SATURATING_PARAMETERS {
        write_or_panic(&mut source, &format!("P{index} int, "));
    }
    source.push_str("Last int) int { return WideLimit }\n");
    source.push_str("func Narrow(n int) int { return NarrowLimit + n }\n");
    source.push_str("func Shadowed(ShadowLimit int) int { return ShadowLimit }\n");

    let file = extract("p/wide.go", &source);

    assert_eq!(diagnostic_codes(&file), NO_DIAGNOSTICS);
    assert!(!has_reference(
        &file,
        "WideLimit",
        ReferenceKind::References
    ));
    assert!(has_reference(
        &file,
        "NarrowLimit",
        ReferenceKind::References
    ));
    assert!(!has_reference(
        &file,
        "ShadowLimit",
        ReferenceKind::References
    ));
}

#[test]
fn python_typing_forms_are_not_unmasked_by_scoped_imports() {
    // A function-local or conditional import of the same name elsewhere in
    // the file must not turn the module's `typing.Literal` back into a type.
    for rebinding in [
        "def unrelated():\n    from app.types import Literal\n",
        "if False:\n    from app.types import Literal\n",
        "def unrelated():\n    import app.types as t\n",
    ] {
        let source = format!(
            "from typing import Literal\nimport typing as t\n{rebinding}\nclass Shape:\n    kind: Literal[VALUE]\n    mode: t.Literal[MODE]\n    size: t.Annotated[Size, LIMIT]\n"
        );

        let file = extract("app/forms.py", &source);

        for value in ["VALUE", "MODE", "LIMIT"] {
            assert!(
                !has_reference(&file, value, ReferenceKind::TypeOf),
                "{value} became a type with {rebinding:?}"
            );
        }
        assert!(has_reference(&file, "Size", ReferenceKind::TypeOf));
    }
    let other = extract(
        "app/forms.py",
        "from app.types import Literal\n\nclass Shape:\n    kind: Literal[RealType]\n",
    );
    assert!(has_reference(&other, "RealType", ReferenceKind::TypeOf));
}

fn assert_optional_facts_omitted(file: &ExtractedFile) {
    assert_eq!(
        diagnostic_codes(file),
        [DiagnosticCode::OptionalFactsOmitted]
    );
}

fn diagnostic_codes(file: &ExtractedFile) -> Vec<DiagnosticCode> {
    file.diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code)
        .collect()
}

fn push_line(source: &mut String, line: &str) {
    source.push_str(line);
    source.push('\n');
}

fn write_or_panic(source: &mut String, text: &str) {
    assert!(
        write!(source, "{text}").is_ok(),
        "writing to a String is infallible"
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = match SourceLimits::new(SOURCE_LIMIT_BYTES) {
        Ok(limits) => limits,
        Err(error) => panic!("source limits are invalid: {error}"),
    };
    let snapshot = match SourceSnapshot::from_bytes(path, source.as_bytes(), limits) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("snapshot failed: {error}"),
    };
    let mut extractor = match NativeExtractor::new(snapshot.language()) {
        Ok(extractor) => extractor,
        Err(error) => panic!("grammar failed: {error}"),
    };
    match extractor.extract(&snapshot) {
        Ok(file) => file,
        Err(error) => panic!("extraction failed: {error}"),
    }
}

fn names_of_kind(file: &ExtractedFile, kind: SymbolKind) -> Vec<&str> {
    file.symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| symbol.name.as_str())
        .collect()
}

fn has_reference(file: &ExtractedFile, name: &str, kind: ReferenceKind) -> bool {
    file.references
        .iter()
        .any(|reference| reference.name == name && reference.kind == kind)
}
