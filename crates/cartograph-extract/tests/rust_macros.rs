//! References inside Rust macro token trees.

mod dependency_ownership;

use cartograph_domain::ReferenceKind;
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedReference, NativeExtractor, RUST_MACRO_RESOLUTION_PREFIX,
    SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT_BYTES: usize = 1024 * 1024;
const MAX_MACRO_REFERENCES: usize = 8_192;

/// The same expressions written directly and as `vec!` arguments.
const EQUIVALENT_SHAPES: &str = r"
mod helpers;

pub struct Worker;

impl Worker {
    pub fn direct(&self, worker: &Worker) {
        let _ = (
            compute(1),
            helpers::render(2),
            crate::helpers::render(3),
            self::local(),
            super::parent(),
            helpers::LIMIT,
            Self::DEFAULT,
            self.measure(),
            self.inner.measure(),
            worker.finish(),
            Some(4),
        );
    }

    pub fn wrapped(&self, worker: &Worker) {
        let _ = vec![
            compute(1),
            helpers::render(2),
            crate::helpers::render(3),
            self::local(),
            super::parent(),
            helpers::LIMIT,
            Self::DEFAULT,
            self.measure(),
            self.inner.measure(),
            worker.finish(),
            Some(4),
        ];
    }
}
";

#[test]
fn macro_arguments_publish_the_same_references_as_direct_code() {
    let file = extract("src/worker.rs", EQUIVALENT_SHAPES);
    let direct = comparable_references(&file, "Worker::direct");
    let wrapped = comparable_references(&file, "Worker::wrapped");
    assert_eq!(direct.len(), 11, "{direct:?}");
    assert_eq!(wrapped, direct);
}

/// The same patterns as `match` arms and as `matches!`/`assert_matches!` arguments.
const EQUIVALENT_PATTERNS: &str = r"
pub fn direct(value: Option<Shape>) -> bool {
    match value {
        Some(Shape::Circle(r)) if radius(r) > 1 => true,
        Some(Shape::Unit) | None => false,
        _ => false,
    }
}

pub fn wrapped(value: Option<Shape>) -> bool {
    assert_matches!(value, Some(Shape::Unit) | None);
    matches!(value, Some(Shape::Circle(r)) if radius(r) > 1)
}

pub fn sugar(callback: Box<dyn Fn(u32) -> u32>) {
    register!(callback as Box<dyn FnMut(u8)>, FnOnce(), dispatch(1));
}
";

#[test]
fn macro_patterns_publish_the_same_references_as_match_arms() {
    let file = extract("src/shapes.rs", EQUIVALENT_PATTERNS);
    let direct = comparable_references(&file, "direct");
    let wrapped = comparable_references(&file, "wrapped");
    // A pattern's tuple-struct or variant path is a path reference, not a
    // call; single-segment patterns such as `Some(r)` publish nothing; the
    // guard after `if` is an ordinary expression.
    assert_eq!(
        direct,
        [
            (ReferenceKind::Calls, "radius".to_owned(), None),
            (ReferenceKind::References, "Shape::Circle".to_owned(), None),
            (ReferenceKind::References, "Shape::Unit".to_owned(), None),
        ]
    );
    assert_eq!(wrapped, direct);
    // `Fn(..)` sugar names a trait in a type; only the real call remains.
    let sugar = comparable_references(&file, "sugar");
    assert_eq!(sugar, [(ReferenceKind::Calls, "dispatch".to_owned(), None)]);
}

/// Turbofish calls written directly and as macro arguments, and macro
/// arguments that follow a turbofish's commas.
const TURBOFISH_SHAPES: &str = r#"
mod helpers;

pub fn direct(items: Items) {
    let _ = (
        convert::<u8, u16>(1),
        helpers::parse::<Vec<u8>, fn(u8) -> u16>(2),
        items.collect::<Vec<_>>(),
    );
}

pub fn wrapped(items: Items) {
    let _ = vec![
        convert::<u8, u16>(1),
        helpers::parse::<Vec<u8>, fn(u8) -> u16>(2),
        items.collect::<Vec<_>>(),
    ];
}

pub fn shifted(flag: bool) {
    assert!(matches!(build::<A, B>(), Shape::Circle(_)));
    assert!(check::<A, B>(flag), "{LIMIT}");
    assert!(1 < 2, "{COMPARED}");
}
"#;

#[test]
fn turbofish_type_arguments_neither_hide_calls_nor_shift_macro_arguments() {
    let file = extract("src/generic.rs", TURBOFISH_SHAPES);
    // A turbofish names no callee: each call names and resolves its function,
    // and a method call dispatches on the member, not on `collect::<..>`.
    assert!(
        file.references
            .iter()
            .all(|reference| !reference.name.contains("::<")),
        "{:?}",
        file.references
    );
    let direct = comparable_references(&file, "direct");
    let calls = direct
        .iter()
        .map(|(kind, name, _)| (*kind, name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            (ReferenceKind::Calls, "convert"),
            (ReferenceKind::Calls, "helpers::parse"),
            (ReferenceKind::Calls, "items.collect"),
        ]
    );
    assert!(direct[2].2.is_some(), "{direct:?}");
    assert_eq!(comparable_references(&file, "wrapped"), direct);
    // The commas inside `::<A, B>` separate type arguments, so the pattern is
    // still `matches!`'s second argument and the format string `assert!`'s;
    // a bare `<` is a comparison and opens nothing.
    let mut expected = vec![
        (ReferenceKind::Calls, "build".to_owned(), None),
        (ReferenceKind::Calls, "check".to_owned(), None),
        (ReferenceKind::References, "COMPARED".to_owned(), None),
        (ReferenceKind::References, "LIMIT".to_owned(), None),
        (ReferenceKind::References, "Shape::Circle".to_owned(), None),
    ];
    expected.sort();
    assert_eq!(comparable_references(&file, "shifted"), expected);
}

#[test]
fn std_format_strings_record_constant_captures_only() {
    let source = r##"
pub fn report(out: &mut String, value: f64, name: &str) {
    println!(
        "{LIMIT} {{ESCAPED}} {0} {} {name} {value:>WIDTH$.PRECISION$} {SHADOWED}\u{FF}",
        1, 2, SHADOWED = 3,
    );
    let _ = write!(out, r#"{RAW_CAPTURE} "{QUOTED}" text"#);
    assert_eq!(name, "{COMPARED_LITERAL}", "{MESSAGE_CAPTURE}");
    custom_log!("{NOT_STD_FORMAT}");
    let _ = format!(b"{BYTE_STRING}");
}
"##;
    let file = extract("src/report.rs", source);
    let values = owned_references(&file, "report")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .collect::<Vec<_>>();
    let names = values
        .iter()
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "LIMIT",
            "WIDTH",
            "PRECISION",
            "RAW_CAPTURE",
            "QUOTED",
            "MESSAGE_CAPTURE"
        ]
    );
    for reference in values {
        assert_span_names(source, reference);
    }
}

#[test]
fn nested_macros_are_macro_calls_with_their_own_format_strings() {
    let source = r#"
pub fn nested() {
    assert_eq!(format!("{INNER_LIMIT}"), helper(vec![leaf(1)]), "{OUTER_MESSAGE}");
}
"#;
    let file = extract("src/nested.rs", source);
    let mut macros = Vec::new();
    let mut others = Vec::new();
    for reference in owned_references(&file, "nested") {
        let expected_macro = format!("{RUST_MACRO_RESOLUTION_PREFIX}{}", reference.name);
        if reference.resolution_name.as_deref() == Some(expected_macro.as_str()) {
            assert_eq!(reference.kind, ReferenceKind::Calls);
            macros.push(reference.name.as_str());
        } else {
            others.push((reference.kind, reference.name.as_str()));
        }
        assert_span_names(source, reference);
    }
    assert_eq!(macros, ["assert_eq", "format", "vec"]);
    assert_eq!(
        others,
        [
            (ReferenceKind::References, "INNER_LIMIT"),
            (ReferenceKind::Calls, "helper"),
            (ReferenceKind::Calls, "leaf"),
            (ReferenceKind::References, "OUTER_MESSAGE"),
        ]
    );
}

#[test]
fn templates_and_non_value_tokens_publish_no_references() {
    let source = r#"
macro_rules! template {
    ($value:expr) => {
        template_call($value) + $crate::template_path::run() + TEMPLATE_LIMIT + format!("{TEMPLATE_CAPTURE}")
    };
}

pub fn declarations() {
    generate! {
        #[cfg(test)]
        #[derive(Debug)]
        fn declared_fn() {}
        struct DeclaredTuple(u8);
        macro_rules! nested_template { ($y:ident) => { nested_template_call($y) + NESTED_LIMIT } }
        for item in (items) { 'OUTER_LABEL: loop {} }
        let shifted = $metavariable(1) + $crate::meta_path::run();
        let quoted = quote_target!(#interpolated(#arguments));
        let text = "string_call(1) STRING_CONST";
        let character = 'c';
        let settings = { "limit": DSL_LIMIT };
    }
}
"#;
    let file = extract("src/templates.rs", source);
    let names = file
        .references
        .iter()
        .map(|reference| (reference.kind, reference.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            (ReferenceKind::Calls, "generate"),
            (ReferenceKind::Calls, "quote_target"),
        ]
    );
}

#[test]
fn constant_tokens_are_values_only_in_std_formatting_expressions() {
    let source = r#"
pub fn constants(x: u8) {
    assert_eq!(x, MAX_ROWS);
    assert!(
        { const LOCAL_LIMIT: u8 = 1; static mut COUNTER: u8 = 0; unsafe { &mut COUNTER }; x < LOCAL_LIMIT },
        "{}",
        N,
    );
    let _ = json!({ "limit": DSL_LIMIT });
    debug_assert!(matches!(x, PATTERN_CONST), "{}", (GROUPED_CONST));
}
"#;
    let file = extract("src/constants.rs", source);
    let values = owned_references(&file, "constants")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .collect::<Vec<_>>();
    let names = values
        .iter()
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["MAX_ROWS", "COUNTER", "LOCAL_LIMIT", "GROUPED_CONST"]
    );
    for reference in &values {
        assert_span_names(source, reference);
    }
    // The uses are recorded, not the declarations that precede them.
    for (reference, use_site) in values[1..3].iter().zip(["&mut COUNTER", "< LOCAL_LIMIT"]) {
        let declaration_skipped = source
            .find(use_site)
            .map(|offset| byte_offset(offset + use_site.len() - reference.name.len()));
        assert_eq!(declaration_skipped, Some(reference.span.start_byte()));
    }
}

#[test]
fn one_macro_invocation_is_bounded_by_its_reference_count() {
    // The comment padding keeps the file's own fact and output budgets above the
    // macro bound, so the boundary below is the macro's.
    let calls = |count: usize| {
        format!(
            "pub fn many() {{ let _ = vec![\n{}]; }}\n",
            "f(), // padding\n".repeat(count)
        )
    };
    let admitted = try_extract("src/many.rs", &calls(MAX_MACRO_REFERENCES))
        .unwrap_or_else(|error| panic!("bounded macro was rejected: {error}"));
    let call_sites = admitted
        .references
        .iter()
        .filter(|reference| reference.name == "f")
        .count();
    assert_eq!(call_sites, MAX_MACRO_REFERENCES);
    assert_eq!(
        try_extract("src/many.rs", &calls(MAX_MACRO_REFERENCES + 1)).err(),
        Some(ExtractError::OutputLimit)
    );

    let captures = |count: usize| {
        format!(
            "pub fn many() {{ print!(\"{}\"); }}\n",
            "{LIMIT} plain text padding ".repeat(count)
        )
    };
    assert!(try_extract("src/many.rs", &captures(MAX_MACRO_REFERENCES)).is_ok());
    assert_eq!(
        try_extract("src/many.rs", &captures(MAX_MACRO_REFERENCES + 1)).err(),
        Some(ExtractError::OutputLimit)
    );
}

fn comparable_references(
    file: &ExtractedFile,
    owner: &str,
) -> Vec<(ReferenceKind, String, Option<String>)> {
    // Signature types and field reads are outside the token-tree contract.
    let mut references = owned_references(file, owner)
        .filter(|reference| {
            matches!(
                reference.kind,
                ReferenceKind::Calls | ReferenceKind::References
            ) && !reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with(RUST_MACRO_RESOLUTION_PREFIX))
        })
        .map(|reference| {
            (
                reference.kind,
                reference.name.clone(),
                reference.resolution_name.clone(),
            )
        })
        .collect::<Vec<_>>();
    references.sort();
    references
}

fn owned_references<'file>(
    file: &'file ExtractedFile,
    owner: &str,
) -> impl Iterator<Item = &'file ExtractedReference> {
    let owner = file
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == owner)
        .map_or_else(
            || panic!("missing owner {owner}"),
            |symbol| symbol.id.clone(),
        );
    file.references
        .iter()
        .filter(move |reference| reference.owner.as_ref() == Some(&owner))
}

/// The span covers exactly the reference name and reports the parser's line and byte column.
fn assert_span_names(source: &str, reference: &ExtractedReference) {
    let start = usize::try_from(reference.span.start_byte())
        .unwrap_or_else(|error| panic!("span start does not fit: {error}"));
    let end = usize::try_from(reference.span.end_byte())
        .unwrap_or_else(|error| panic!("span end does not fit: {error}"));
    let text = source
        .get(start..end)
        .unwrap_or_else(|| panic!("invalid span for {}", reference.name));
    assert_eq!(text, reference.name);
    let prefix = &source[..start];
    let line = prefix.matches('\n').count() + 1;
    let column = prefix
        .rfind('\n')
        .map_or(start, |newline| start - newline - 1);
    assert_eq!(
        (
            usize::try_from(reference.span.start_line()).ok(),
            usize::try_from(reference.span.start_column()).ok()
        ),
        (Some(line), Some(column)),
        "{}",
        reference.name
    );
}

fn byte_offset(index: usize) -> u64 {
    u64::try_from(index).unwrap_or_else(|error| panic!("offset does not fit: {error}"))
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    try_extract(path, source).unwrap_or_else(|error| panic!("extraction failed: {error}"))
}

fn try_extract(path: &str, source: &str) -> Result<ExtractedFile, ExtractError> {
    let limits = SourceLimits::new(SOURCE_LIMIT_BYTES)
        .unwrap_or_else(|error| panic!("source limits are invalid: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
    NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("grammar failed: {error}"))
        .extract(&snapshot)
}
