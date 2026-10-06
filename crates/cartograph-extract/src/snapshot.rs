use std::{fmt, str};

use cartograph_domain::{ContentDigest, FileId, NormalizedPath, SourceLanguage};
use thiserror::Error;

use crate::identity::file_id;

const MAX_CONFIGURED_SOURCE_BYTES: usize = 512 * 1024 * 1024;

/// Validated per-file source-size policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLimits {
    max_source_bytes: usize,
}

impl SourceLimits {
    /// Require a nonzero source bound no larger than the hard parser ceiling.
    /// # Errors
    ///
    /// Returns an error if `max_source_bytes` is zero or above the parser hard ceiling.
    pub const fn new(max_source_bytes: usize) -> Result<Self, SourceLimitsError> {
        if max_source_bytes == 0 || max_source_bytes > MAX_CONFIGURED_SOURCE_BYTES {
            return Err(SourceLimitsError);
        }
        Ok(Self { max_source_bytes })
    }

    /// Maximum accepted exact source bytes.
    #[must_use]
    pub const fn max_source_bytes(self) -> usize {
        self.max_source_bytes
    }
}

/// A configured source-size policy was zero or exceeded the hard ceiling.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("native source-size limit is invalid")]
pub struct SourceLimitsError;

/// Safe immutable input to one native parser invocation.
#[derive(PartialEq, Eq)]
pub struct SourceSnapshot {
    path: NormalizedPath,
    language: SourceLanguage,
    file_id: FileId,
    content_hash: ContentDigest,
    byte_size: u64,
    line_count: u32,
    source: Box<str>,
}

struct ValidatedSource {
    path: NormalizedPath,
    language: SourceLanguage,
    byte_size: u64,
}

struct ValidatedSourcePath {
    path: NormalizedPath,
    byte_size: u64,
}

#[derive(Clone, Copy)]
struct SnapshotBytes<'a> {
    raw_path: &'a str,
    bytes: &'a [u8],
    limits: SourceLimits,
}

impl<'a> SnapshotBytes<'a> {
    const fn new(raw_path: &'a str, bytes: &'a [u8], limits: SourceLimits) -> Self {
        Self {
            raw_path,
            bytes,
            limits,
        }
    }
}

pub(crate) struct StreamedSource {
    source: String,
    content_hash: ContentDigest,
}

impl StreamedSource {
    pub(crate) const fn new(source: String, content_hash: ContentDigest) -> Self {
        Self {
            source,
            content_hash,
        }
    }
}

impl SourceSnapshot {
    /// Validate path, extension, size, and UTF-8 before any parser is invoked.
    /// # Errors
    ///
    /// Returns an error for an invalid/unsupported normalized path, oversized
    /// or non-UTF-8 bytes, line-count overflow, or bounded allocation failure.
    pub fn from_bytes(
        raw_path: &str,
        bytes: &[u8],
        limits: SourceLimits,
    ) -> Result<Self, SnapshotError> {
        Self::from_bytes_with_classifier(
            SnapshotBytes::new(raw_path, bytes, limits),
            classify_source,
        )
    }

    /// Build a bounded snapshot for an implemented language before production admission.
    ///
    /// This bypasses only the native-indexable registry filter; path normalization, exact size,
    /// UTF-8, hashing, and the canonical v1 path/content classifier remain enforced. Production
    /// discovery and indexing must use [`Self::from_bytes`].
    /// # Errors
    ///
    /// Returns an error for an invalid or unimplemented known-language path,
    /// oversized/non-UTF-8 bytes, line-count overflow, or allocation failure.
    pub fn from_bytes_for_capability_validation(
        raw_path: &str,
        bytes: &[u8],
        limits: SourceLimits,
    ) -> Result<Self, SnapshotError> {
        Self::from_bytes_with_classifier(
            SnapshotBytes::new(raw_path, bytes, limits),
            classify_known_source,
        )
    }

    fn from_bytes_with_classifier(
        input: SnapshotBytes<'_>,
        classifier: fn(ValidatedSourcePath, &str) -> Result<ValidatedSource, SnapshotError>,
    ) -> Result<Self, SnapshotError> {
        let SnapshotBytes {
            raw_path,
            bytes,
            limits,
        } = input;
        let validated_path = validate_source_path(raw_path, bytes.len(), limits)?;
        let validated_utf8 = str::from_utf8(bytes).map_err(|_| SnapshotError::InvalidUtf8)?;
        let validated = classifier(validated_path, validated_utf8)?;
        let mut source = String::new();
        source
            .try_reserve(validated_utf8.len())
            .map_err(|_| SnapshotError::ResourceLimit)?;
        source.push_str(validated_utf8);
        let content_hash = ContentDigest::from_bytes(*blake3::hash(bytes).as_bytes());
        Ok(Self::build(
            validated,
            content_hash,
            source.into_boxed_str(),
        ))
    }

    pub(crate) fn from_streamed_source(
        raw_path: &str,
        streamed: StreamedSource,
        limits: SourceLimits,
    ) -> Result<Self, SnapshotError> {
        let validated_path = validate_source_path(raw_path, streamed.source.len(), limits)?;
        let validated = classify_source(validated_path, &streamed.source)?;
        Ok(Self::build(
            validated,
            streamed.content_hash,
            streamed.source.into_boxed_str(),
        ))
    }

    fn build(validated: ValidatedSource, content_hash: ContentDigest, source: Box<str>) -> Self {
        let file_id = file_id(&validated.path);
        let line_count = source
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            .saturating_add(1);
        let line_count = u32::try_from(line_count).unwrap_or(u32::MAX);
        Self {
            path: validated.path,
            language: validated.language,
            file_id,
            content_hash,
            byte_size: validated.byte_size,
            line_count,
            source,
        }
    }

    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Native grammar selected from the canonical extension.
    #[must_use]
    pub const fn language(&self) -> SourceLanguage {
        self.language
    }

    /// Stable path-derived file identity.
    #[must_use]
    pub const fn file_id(&self) -> &FileId {
        &self.file_id
    }

    /// Digest of exact original source bytes.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentDigest {
        &self.content_hash
    }

    /// Original source size.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Exact one-based source line count, including an empty final line after a newline.
    #[must_use]
    pub const fn line_count(&self) -> u32 {
        self.line_count
    }

    /// Validated UTF-8 source, byte-identical to the hashed input.
    #[must_use]
    pub const fn source(&self) -> &str {
        &self.source
    }

    /// Consume the snapshot and transfer its validated UTF-8 source allocation.
    #[must_use]
    pub fn into_source(self) -> Box<str> {
        self.source
    }

    /// Copy this snapshot as a view parsed under an embedded script dialect.
    ///
    /// Path, file identity, content digest, and bytes stay identical, so every
    /// span and symbol identity extracted over the view is a fact of this file.
    /// Only the language differs; the view lives for one extraction pass and is
    /// never published.
    pub(crate) fn dialect_view(
        &self,
        language: SourceLanguage,
    ) -> Result<Self, crate::ExtractError> {
        let mut source = String::new();
        source
            .try_reserve_exact(self.source.len())
            .map_err(|_| crate::ExtractError::OutputLimit)?;
        source.push_str(&self.source);
        Ok(Self {
            path: self.path.clone(),
            language,
            file_id: self.file_id.clone(),
            content_hash: self.content_hash.clone(),
            byte_size: self.byte_size,
            line_count: self.line_count,
            source: source.into_boxed_str(),
        })
    }
}

impl fmt::Debug for SourceSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceSnapshot")
            .field("language", &self.language)
            .field("file_id", &self.file_id)
            .field("content_hash", &self.content_hash)
            .field("byte_size", &self.byte_size)
            .finish_non_exhaustive()
    }
}

fn validate_source_path(
    raw_path: &str,
    byte_length: usize,
    limits: SourceLimits,
) -> Result<ValidatedSourcePath, SnapshotError> {
    let path = NormalizedPath::parse(raw_path).map_err(|_| SnapshotError::InvalidPath)?;
    if byte_length > limits.max_source_bytes() {
        return Err(SnapshotError::SourceTooLarge);
    }
    let byte_size = u64::try_from(byte_length).map_err(|_| SnapshotError::SourceTooLarge)?;
    Ok(ValidatedSourcePath { path, byte_size })
}

fn classify_source(
    validated: ValidatedSourcePath,
    source: &str,
) -> Result<ValidatedSource, SnapshotError> {
    let language = SourceLanguage::for_normalized_path_with_source(validated.path.as_str(), source)
        .ok_or(SnapshotError::UnsupportedLanguage)?;
    Ok(ValidatedSource {
        path: validated.path,
        language,
        byte_size: validated.byte_size,
    })
}

fn classify_known_source(
    validated: ValidatedSourcePath,
    source: &str,
) -> Result<ValidatedSource, SnapshotError> {
    let language = SourceLanguage::detect(validated.path.as_str(), Some(source))
        .ok_or(SnapshotError::UnsupportedLanguage)?;
    Ok(ValidatedSource {
        path: validated.path,
        language,
        byte_size: validated.byte_size,
    })
}

/// Source bytes were unsafe or unsupported for the first native extractor slice.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    /// The path was absolute, escaping, empty, or oversized.
    #[error("native source path is invalid")]
    InvalidPath,
    /// The extension is not owned by a native grammar slice.
    #[error("native source language is unsupported")]
    UnsupportedLanguage,
    /// Exact input bytes exceeded the configured bound.
    #[error("native source exceeds the configured size limit")]
    SourceTooLarge,
    /// Native parser slices accept UTF-8 source only.
    #[error("native source is not valid UTF-8")]
    InvalidUtf8,
    /// A bounded snapshot buffer could not be reserved without aborting the process.
    #[error("native source snapshot buffer could not be reserved")]
    ResourceLimit,
}

/// True when a supported source path uses a test directory or language-owned test filename.
#[must_use]
pub fn is_test_source_path(path: &str) -> bool {
    let Some(language) = classify_extension(path) else {
        return false;
    };
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_some()
            && matches_ignore_ascii_case(
                component,
                &[
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
                ],
            )
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
        SourceLanguage::Python | SourceLanguage::Rust => {
            stem.starts_with("test_") || stem.ends_with("_test")
        }
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

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn classify_extension(path: &str) -> Option<SourceLanguage> {
    SourceLanguage::for_normalized_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_routing_covers_supported_extensions_directories_and_case() {
        for value in [
            "tests/root.ts",
            "test/root.mts",
            "src/__tests__/nested.cts",
            "src/component.test.tsx",
            "src/module.spec.js",
            "src/module.test.mjs",
            "src/module.spec.cjs",
            "src/module.test.jsx",
            "src/module.spec.xsjs",
            "src/module.test.xsjslib",
            "tests/native_test.rs",
            "tests/native_test.py",
            "tests/native_test.pyi",
            "tests/native_test.go",
            "pkg/worker_test.go",
            "python/test_worker.py",
            "python/worker_test.py",
            "python/test_types.pyi",
            "SRC/TESTS/Mixed.SPEC.TS",
            "docs/test-beds/typescript/fixture.ts",
            "src/fixtures/example.rs",
            "pkg/testdata/input.go",
        ] {
            assert!(is_test_source_path(value), "{value}");
        }
        assert!(!is_test_source_path("src/contest.ts"));
        assert!(!is_test_source_path("src/unit.test.md"));
        assert!(!is_test_source_path("src/worker_test.ts"));
        assert!(!is_test_source_path("src/test_worker.ts"));
        assert!(is_test_source_path("src/worker_test.rs"));
        assert!(is_test_source_path("spec/service_spec.rb"));
        assert!(is_test_source_path("src/OrderServiceTest.java"));
        assert!(is_test_source_path("src/CacheSpec.kt"));
        assert!(!is_test_source_path("src/Contest.java"));
    }
}
