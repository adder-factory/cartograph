//! Full-fact assertions shared by the credential-screening regressions.

use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

pub const CREDENTIAL_INPUTS: [&str; 20] = [
    "https://reader:FAKEPASSWORDxyz@example.invalid/module",
    "https://reader:FAKEPASSWORDxyz.local@example.invalid/module",
    "https://example.invalid/module?token=FAKEQUERYabcdef",
    "./sk_live_FAKE1234567890abcdef/module",
    "./glpat-aaaaaaaaaaaaaaaaaaaa/module",
    "sk_live_FAKE1234567890abcdef",
    "glpat-aaaaaaaaaaaaaaaaaaaa",
    "Glpat-aaaaaaaaaaaaaaaaaaaa",
    "SK_LIVE_FAKE1234567890abcdef",
    "https://example.invalid/module#access_token=FAKEQUERYabcdef",
    "https://example.invalid/module?password=FAKEQUERYabcdef",
    "https://Zx9mQ2vL8kP4rT6yW1nB3cD5@example.invalid/module",
    "//reader:FAKEPASSWORDxyz@example.invalid/module",
    "https://FAKEQUERYabcdef_token:@example.invalid/module",
    "https://example.invalid/module?client-secret=FAKEQUERYabcdef",
    "https://example.invalid/module?api%5Fkey=FAKEQUERYabcdef",
    "https://example.invalid/module#client-secret=FAKEQUERYabcdef",
    "https://example.invalid/module#api%5Fkey=FAKEQUERYabcdef",
    "https://example.invalid/module?token_%FF=FAKEQUERYabcdef",
    "https://example.invalid/module?api%ZZkey=FAKEQUERYabcdef",
];

pub fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(1024 * 1024)
        .unwrap_or_else(|error| panic!("valid source limit: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("valid credential regression input: {error}"));
    NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("supported regression language: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("credential regression extraction succeeds: {error}"))
}

pub fn assert_no_credentials(file: &ExtractedFile) {
    let facts = serde_json::to_string(file)
        .unwrap_or_else(|error| panic!("serialize every extracted fact: {error}"))
        .to_ascii_lowercase();
    for secret in [
        "FAKEPASSWORDxyz",
        "FAKEQUERYabcdef",
        "FAKE1234567890abcdef",
        "aaaaaaaaaaaaaaaaaaaa",
        "Zx9mQ2vL8kP4rT6yW1nB3cD5",
    ] {
        assert!(
            !facts.contains(&secret.to_ascii_lowercase()),
            "{} retained a credential in extracted facts: {facts}",
            file.path.as_str()
        );
    }
}

pub fn assert_screened(path: &str, template: &str, ordinary: &str) {
    for value in CREDENTIAL_INPUTS {
        let file = extract(path, &template.replace("@VALUE@", value));
        assert_no_credentials(&file);
    }
    let file = extract(path, &template.replace("@VALUE@", ordinary));
    assert!(
        file.test_search_text.contains(ordinary)
            || file
                .symbols
                .iter()
                .any(|symbol| symbol.name.contains(ordinary)
                    || symbol
                        .docstring
                        .as_deref()
                        .is_some_and(|text| text.contains(ordinary)))
            || file
                .references
                .iter()
                .any(|reference| reference.name == ordinary)
            || file
                .import_bindings
                .iter()
                .any(|binding| binding.module_specifier == ordinary),
        "{path} must retain the ordinary literal {ordinary}: {file:?}"
    );
}
