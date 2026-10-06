use std::collections::BTreeSet;

use crate::{
    ExtractError, ExtractedFile, SourceSnapshot, budget::native_output_limit, is_test_source_path,
};

const MAXIMUM_TEST_SEARCH_BYTES: usize = 16 * 1_024;
const MAXIMUM_TEST_TITLE_BYTES: usize = 256;

pub(crate) fn enrich(
    snapshot: &SourceSnapshot,
    mut extracted: ExtractedFile,
) -> Result<ExtractedFile, ExtractError> {
    if !is_test_source_path(snapshot.path().as_str()) && !extracted.has_inline_tests {
        return Ok(extracted);
    }
    let mut names = literal_test_names(snapshot.source());
    names.extend(
        extracted
            .symbols
            .iter()
            .filter(|symbol| {
                matches!(
                    symbol.kind.as_str(),
                    "function" | "method" | "class" | "component"
                )
            })
            .map(|symbol| bounded(&symbol.qualified_name, MAXIMUM_TEST_TITLE_BYTES)),
    );
    let mut text = String::new();
    for name in names {
        let separator = usize::from(!text.is_empty());
        let Some(next) = text
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(name.len()))
        else {
            extracted.test_search_truncated = true;
            break;
        };
        if next > MAXIMUM_TEST_SEARCH_BYTES {
            extracted.test_search_truncated = true;
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&name);
    }
    extracted.test_search_text = text;
    let output_limit =
        native_output_limit(snapshot.byte_size()).ok_or(ExtractError::OutputLimit)?;
    if extracted.modeled_retained_bytes() > output_limit {
        return Err(ExtractError::OutputLimit);
    }
    Ok(extracted)
}

fn literal_test_names(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut names = BTreeSet::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index + 2);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index + 2);
            }
            quote @ (b'\'' | b'"' | b'`') => index = skip_quoted(bytes, index + 1, quote),
            byte if identifier_start(byte) => {
                index = scan_test_identifier(source, index, &mut names);
            }
            _ => index = index.saturating_add(1),
        }
    }
    names
}

fn scan_test_identifier(source: &str, start: usize, names: &mut BTreeSet<String>) -> usize {
    let bytes = source.as_bytes();
    let mut end = start.saturating_add(1);
    while bytes
        .get(end)
        .is_some_and(|byte| identifier_continue(*byte))
    {
        end = end.saturating_add(1);
    }
    if !matches!(
        &source[start..end],
        "describe" | "context" | "suite" | "it" | "test" | "specify" | "scenario"
    ) {
        return end;
    }
    let Some((title_start, title_end)) = invocation_title(bytes, end) else {
        return end;
    };
    let raw = &source[title_start..title_end];
    if raw
        .split_whitespace()
        .any(crate::walk::specifier_safety::specifier_may_carry_credential)
    {
        return end;
    }
    let title = clean_title(raw);
    if !title.is_empty() {
        names.insert(bounded(&title, MAXIMUM_TEST_TITLE_BYTES));
    }
    end
}

fn invocation_title(bytes: &[u8], mut index: usize) -> Option<(usize, usize)> {
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
        } else if byte == quote {
            return Some((start, cursor));
        } else if matches!(byte, b'\n' | b'\r') && quote != b'`' {
            return None;
        } else {
            cursor = cursor.saturating_add(1);
        }
    }
    None
}

fn clean_title(raw: &str) -> String {
    raw.chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn bounded(value: &str, maximum: usize) -> String {
    value[..value.floor_char_boundary(maximum)].to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_names_exclude_comments_and_unrelated_literals() {
        let names = literal_test_names(
            "// test('hidden', () => {});\nconst x = \"it('also hidden')\";\n\
             describe('auth', () => { it.only('rejects expiry', () => {}); });\n",
        );
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["auth", "rejects expiry"]
        );
    }
}
