use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ContentDigest, GenerationId, NormalizedPath, ProjectId};
use cartograph_extract::{NativeExtractor, SourceReadOptions, SourceRoot};
use memchr::memchr_iter;
use serde::Serialize;
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime, source_limits};

const MAXIMUM_TEST_FILES: usize = 500;
const MAXIMUM_CASES_PER_FILE: u16 = 100;
const MAXIMUM_TEST_TITLE_BYTES: usize = 256;

/// Validated test-name evidence bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestEvidenceOptions {
    cases_per_file: u16,
}

impl TestEvidenceOptions {
    /// Creates validated test-evidence limits.
    ///
    /// # Errors
    ///
    /// Returns [`TestEvidenceError::InvalidOptions`] when `cases_per_file` is
    /// zero or exceeds the retained cases allowed for one test file.
    pub const fn new(cases_per_file: u16) -> Result<Self, TestEvidenceError> {
        if cases_per_file == 0 || cases_per_file > MAXIMUM_CASES_PER_FILE {
            return Err(TestEvidenceError::InvalidOptions);
        }
        Ok(Self { cases_per_file })
    }
}

/// One fresh test title or declaration mined from an affected test file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCaseEvidence {
    line: u32,
    name: String,
    kind: &'static str,
    provenance: &'static str,
}

/// Bounded test names for one exact current-generation file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFileEvidence {
    path: NormalizedPath,
    content_hash: ContentDigest,
    case_count: u64,
    cases_truncated: bool,
    cases: Vec<TestCaseEvidence>,
}

/// Fresh evidence for every requested affected test path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestEvidenceReport {
    generation_id: GenerationId,
    files: Vec<TestFileEvidence>,
}

/// Path-, source-, and credential-safe test-evidence failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum TestEvidenceError {
    #[error("test evidence options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("test evidence requires a current generation")]
    /// No current indexed generation is available.
    NotIndexed,
    #[error("test evidence storage is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("test evidence source is unavailable")]
    /// Required source evidence could not be read safely.
    SourceUnavailable,
    #[error("test evidence source or generation changed")]
    /// Live source no longer matches the generation or digest fence.
    SourceChanged,
    #[error("test evidence was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
}

impl ProjectRuntime {
    /// Mine fresh test titles/declarations for exact affected paths without
    /// persisting source literals. Every file hash must still match the
    /// immutable current generation.
    /// # Errors
    ///
    /// Returns an error when the path set is empty or excessive, the requested
    /// project/generation is not current, fingerprints or source cannot be read,
    /// source/generation identity changes, or cancellation wins.
    pub async fn test_evidence(
        &self,
        project_id: ProjectId,
        generation_id: GenerationId,
        paths: Vec<NormalizedPath>,
        options: TestEvidenceOptions,
        cancellation: ProjectCancellation,
    ) -> Result<TestEvidenceReport, TestEvidenceError> {
        if paths.is_empty() || paths.len() > MAXIMUM_TEST_FILES {
            return Err(TestEvidenceError::InvalidOptions);
        }
        let before = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| TestEvidenceError::StorageUnavailable)?
            .ok_or(TestEvidenceError::NotIndexed)?;
        let current = before
            .current
            .as_ref()
            .ok_or(TestEvidenceError::NotIndexed)?;
        if before.project_id != project_id || current.generation_id != generation_id {
            return Err(TestEvidenceError::SourceChanged);
        }
        let requested = paths.into_iter().collect::<BTreeSet<_>>();
        let fingerprints = self
            .database()
            .current_file_fingerprints(&project_id)
            .await
            .map_err(|_| TestEvidenceError::StorageUnavailable)?
            .into_iter()
            .filter(|fingerprint| requested.contains(fingerprint.path()))
            .map(|fingerprint| {
                (
                    fingerprint.path().clone(),
                    fingerprint.content_hash().clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if fingerprints.len() != requested.len() {
            return Err(TestEvidenceError::SourceChanged);
        }
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(TestEvidenceError::Cancelled),
            result = self.source_scan_permits.clone().acquire_owned() => {
                result.map_err(|_| TestEvidenceError::SourceUnavailable)?
            }
        };
        let root = self.root.clone();
        let worker_cancellation = cancellation.clone();
        let files = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            scan_test_evidence(&root, fingerprints, options, || {
                worker_cancellation.is_cancelled()
            })
        })
        .await
        .map_err(|_| TestEvidenceError::SourceUnavailable)??;
        if cancellation.is_cancelled() {
            return Err(TestEvidenceError::Cancelled);
        }
        let after = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| TestEvidenceError::StorageUnavailable)?;
        let unchanged = after.as_ref().is_some_and(|snapshot| {
            snapshot.project_id == project_id
                && snapshot.current.as_ref().is_some_and(|observed| {
                    observed.generation_id == generation_id
                        && observed.source_revision == current.source_revision
                })
        });
        if !unchanged {
            return Err(TestEvidenceError::SourceChanged);
        }
        Ok(TestEvidenceReport {
            generation_id,
            files,
        })
    }
}

fn scan_test_evidence<Cancel>(
    root: &std::path::Path,
    fingerprints: BTreeMap<NormalizedPath, ContentDigest>,
    options: TestEvidenceOptions,
    mut cancelled: Cancel,
) -> Result<Vec<TestFileEvidence>, TestEvidenceError>
where
    Cancel: FnMut() -> bool,
{
    let source_root = SourceRoot::open(root).map_err(|_| TestEvidenceError::SourceUnavailable)?;
    let limits = source_limits().map_err(|_| TestEvidenceError::SourceUnavailable)?;
    let mut files = Vec::with_capacity(fingerprints.len());
    for (path, expected_hash) in fingerprints {
        if cancelled() {
            return Err(TestEvidenceError::Cancelled);
        }
        let snapshot = source_root
            .read_with_cancellation(&path, SourceReadOptions::new(limits, &mut cancelled))
            .map_err(|_| TestEvidenceError::SourceUnavailable)?;
        if snapshot.content_hash() != &expected_hash {
            return Err(TestEvidenceError::SourceChanged);
        }
        let mut cases = literal_test_cases(snapshot.source());
        if let Ok(mut extractor) = NativeExtractor::new(snapshot.language())
            && let Ok(extracted) = extractor.extract_with_cancellation(&snapshot, &mut cancelled)
        {
            for symbol in extracted.symbols.iter().filter(|symbol| {
                matches!(
                    symbol.kind.as_str(),
                    "function" | "method" | "class" | "component"
                )
            }) {
                cases.insert(TestCaseEvidence {
                    line: symbol.span.start_line(),
                    name: bounded_title(&symbol.qualified_name),
                    kind: "declaration",
                    provenance: "native_test_file_symbol",
                });
            }
        }
        let case_count = u64::try_from(cases.len()).unwrap_or(u64::MAX);
        let mut cases = cases.into_iter().collect::<Vec<_>>();
        cases.truncate(usize::from(options.cases_per_file));
        files.push(TestFileEvidence {
            path,
            content_hash: expected_hash,
            case_count,
            cases_truncated: case_count > u64::from(options.cases_per_file),
            cases,
        });
    }
    Ok(files)
}

fn literal_test_cases(source: &str) -> BTreeSet<TestCaseEvidence> {
    let bytes = source.as_bytes();
    let mut cases = BTreeSet::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index + 2);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index + 2);
            }
            quote @ (b'\'' | b'"' | b'`') => {
                index = skip_quoted(bytes, index + 1, quote);
            }
            byte if identifier_start(byte) => {
                index = scan_literal_test_identifier(source, bytes, index, &mut cases);
            }
            _ => index = index.saturating_add(1),
        }
    }
    cases
}

fn scan_literal_test_identifier(
    source: &str,
    bytes: &[u8],
    start: usize,
    cases: &mut BTreeSet<TestCaseEvidence>,
) -> usize {
    let mut end = start.saturating_add(1);
    while bytes
        .get(end)
        .is_some_and(|byte| identifier_continue(*byte))
    {
        end = end.saturating_add(1);
    }
    let identifier = &source[start..end];
    if !matches!(
        identifier,
        "describe" | "context" | "suite" | "it" | "test" | "specify" | "scenario"
    ) {
        return end;
    }
    let Some((title_start, title_end, quote)) = invocation_title(bytes, end) else {
        return end;
    };
    let title = clean_title(&source[title_start..title_end], quote);
    if title.is_empty() {
        return end;
    }
    let kind = if matches!(identifier, "describe" | "context" | "suite") {
        "suite"
    } else {
        "test_case"
    };
    cases.insert(TestCaseEvidence {
        line: one_based_line(bytes, start),
        name: bounded_title(&title),
        kind,
        provenance: "literal_test_invocation",
    });
    end
}

fn invocation_title(bytes: &[u8], mut index: usize) -> Option<(usize, usize, u8)> {
    index = skip_ascii_space(bytes, index);
    if bytes.get(index) == Some(&b'.') {
        index = index.saturating_add(1);
        while bytes
            .get(index)
            .is_some_and(|byte| identifier_continue(*byte))
        {
            index = index.saturating_add(1);
        }
        index = skip_ascii_space(bytes, index);
    }
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    index = skip_ascii_space(bytes, index.saturating_add(1));
    let quote = *bytes.get(index)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let start = index.saturating_add(1);
    let mut cursor = start;
    while let Some(byte) = bytes.get(cursor).copied() {
        if byte == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if byte == quote {
            return Some((start, cursor, quote));
        }
        if matches!(byte, b'\n' | b'\r') && quote != b'`' {
            return None;
        }
        cursor = cursor.saturating_add(1);
    }
    None
}

fn clean_title(raw: &str, quote: u8) -> String {
    let mut output = String::new();
    let mut escaped = false;
    for character in raw.chars() {
        if escaped {
            output.push(match character {
                'n' | 'r' | 't' => ' ',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote == b'`' && character == '$' {
            output.push('$');
        } else if !character.is_control() {
            output.push(character);
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bounded_title(title: &str) -> String {
    title[..title.floor_char_boundary(MAXIMUM_TEST_TITLE_BYTES)].to_owned()
}

fn skip_ascii_space(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index = index.saturating_add(1);
    }
    index
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
        index = index.saturating_add(1);
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'*') && bytes.get(index + 1) == Some(&b'/') {
            return index.saturating_add(2);
        }
        index = index.saturating_add(1);
    }
    index
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    while let Some(byte) = bytes.get(index).copied() {
        if byte == b'\\' {
            index = index.saturating_add(2);
        } else if byte == quote {
            return index.saturating_add(1);
        } else {
            index = index.saturating_add(1);
        }
    }
    index
}

const fn identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

const fn identifier_continue(byte: u8) -> bool {
    identifier_start(byte) || byte.is_ascii_digit()
}

fn one_based_line(bytes: &[u8], offset: usize) -> u32 {
    u32::try_from(memchr_iter(b'\n', bytes.get(..offset).unwrap_or_default()).count())
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_test_mining_skips_comments_and_unrelated_strings() {
        let source = r#"
            // test('commented out', () => {});
            const sample = "describe('inside a string')";
            describe('authentication', () => {
              it.only("rejects expired tokens", () => {});
              test(`parses Cookie header`, () => {});
            });
        "#;
        let cases = literal_test_cases(source);
        let names = cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "authentication",
                "rejects expired tokens",
                "parses Cookie header"
            ]
        );
    }
}
