use super::SourceLanguage;

/// True when a supported source path uses a test directory or language-owned test filename.
///
/// Extraction, retrieval preferences, and affected-test selection share this policy. Both
/// slash styles are accepted; ordinary names such as `contest` and `testing_helpers` do not
/// become tests merely because they contain the word `test`.
#[must_use]
pub fn is_test_source_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let Some(language) = SourceLanguage::for_normalized_path(&normalized) else {
        return false;
    };
    let mut components = normalized.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_some()
            && [
                "test",
                "tests",
                "__tests__",
                "spec",
                "specs",
                "__specs__",
                "fixture",
                "fixtures",
                "test-bed",
                "test-beds",
                "testdata",
            ]
            .iter()
            .any(|candidate| component.eq_ignore_ascii_case(candidate))
        {
            return true;
        }
        if components.peek().is_none() {
            return is_test_filename(language, component);
        }
    }
    false
}

fn is_test_filename(language: SourceLanguage, filename: &str) -> bool {
    let raw_stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let stem = raw_stem.to_ascii_lowercase();
    match language {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => stem
            .rsplit_once('.')
            .is_some_and(|(_, suffix)| matches!(suffix, "test" | "spec")),
        SourceLanguage::Rust => {
            matches!(stem.as_str(), "test" | "tests")
                || stem.starts_with("test_")
                || stem.ends_with("_test")
        }
        SourceLanguage::Python => stem.starts_with("test_") || stem.ends_with("_test"),
        SourceLanguage::Go => stem.ends_with("_test"),
        SourceLanguage::Ruby => stem.ends_with("_spec") || stem.ends_with("_test"),
        SourceLanguage::Java
        | SourceLanguage::Kotlin
        | SourceLanguage::CSharp
        | SourceLanguage::Swift => {
            raw_stem
                .strip_suffix("Tests")
                .or_else(|| raw_stem.strip_suffix("Test"))
                .is_some_and(|subject| !subject.is_empty())
                || matches!(language, SourceLanguage::Kotlin | SourceLanguage::Swift)
                    && raw_stem
                        .strip_suffix("Spec")
                        .is_some_and(|subject| !subject.is_empty())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_test_source_path;

    #[test]
    fn shared_test_policy_covers_leaf_modules_languages_and_both_slash_styles() {
        for path in [
            "src/cache/tests.rs",
            "src/test.rs",
            "SRC/TESTS.RS",
            "src\\cache\\tests.rs",
            "src/worker_test.rs",
            "src/test_worker.py",
            "pkg/worker_test.go",
            "spec/service_spec.rb",
            "src/component.test.tsx",
            "src/module.spec.js",
            "src/OrderServiceTest.java",
            "src/CacheSpec.kt",
            "src/fixtures/example.rs",
            "pkg/testdata/input.go",
            "docs/test-beds/typescript/fixture.ts",
        ] {
            assert!(is_test_source_path(path), "{path}");
        }
    }

    #[test]
    fn shared_test_policy_preserves_language_boundaries_and_non_test_names() {
        for path in [
            "src/contest.rs",
            "src/tests_helper.rs",
            "src/testing_helpers.ts",
            "src/test.ts",
            "src/tests.py",
            "src/worker_test.ts",
            "src/test_worker.ts",
            "src/Contest.java",
            "src/cache.rs",
            "tests/unit.test.md",
            "tests/unknown.extension",
        ] {
            assert!(!is_test_source_path(path), "{path}");
        }
    }
}
