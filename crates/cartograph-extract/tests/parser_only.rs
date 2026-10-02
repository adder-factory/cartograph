//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, SourceLanguage};
use cartograph_extract::{NativeExtractor, SnapshotError, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn parser_only_modes_parse_and_diagnose_without_fake_structural_facts() {
    let fixtures = [
        (
            "styles/site.css",
            "body { color: red; }",
            SourceLanguage::Css,
        ),
        (
            "views/user.erb",
            "<div><%= user.name %></div>",
            SourceLanguage::EmbeddedTemplate,
        ),
        (
            "docs/api.jsdoc",
            "/** Adds one. */\n",
            SourceLanguage::JsDoc,
        ),
        (
            "config/app.json",
            r#"{"enabled":true}"#,
            SourceLanguage::Json,
        ),
        (
            "notebooks/demo.ipynb",
            r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
            SourceLanguage::Jupyter,
        ),
        (
            "patterns/email.regex",
            r"[a-z]+@[a-z]+\.[a-z]+",
            SourceLanguage::Regex,
        ),
    ];

    for (path, source, language) in fixtures {
        let snapshot = snapshot(path, source);
        assert_eq!(snapshot.language(), language);
        let mut extractor = NativeExtractor::new(language)
            .unwrap_or_else(|error| panic!("{path} parser was unavailable: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"));
        assert_eq!(extracted.parse_status, FileParseStatus::Parsed, "{path}");
        assert!(extracted.symbols.is_empty(), "{path}");
        assert!(extracted.containments.is_empty(), "{path}");
        assert!(extracted.references.is_empty(), "{path}");
        assert!(extracted.import_bindings.is_empty(), "{path}");
        assert!(extracted.diagnostics.is_empty(), "{path}");
    }
}

#[test]
fn parser_only_syntax_damage_is_partial_and_unknown_extensions_fail_closed() {
    let snapshot = snapshot("config/broken.json", r#"{"broken": }"#);
    let mut extractor = NativeExtractor::new(SourceLanguage::Json)
        .unwrap_or_else(|error| panic!("JSON parser was unavailable: {error}"));
    let extracted = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("damaged JSON extraction failed: {error}"));
    assert_eq!(extracted.parse_status, FileParseStatus::Partial);
    assert_ne!(extracted.diagnostics, []);
    assert_eq!(extracted.symbols, []);

    assert_eq!(
        SourceSnapshot::from_bytes("src/main.unknown", b"class Main {}", limits()).err(),
        Some(SnapshotError::UnsupportedLanguage)
    );
}

fn snapshot(path: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("test source limit was invalid: {error}"))
}
