//! BG3 identifier rules shared by the resource, stats, and Anubis scanners:
//! canonical UUIDs, `Name_<uuid>` GUIDSTRINGs, `h…` localization handles,
//! and the identifier tokens a field or script string refers to.

use crate::walk::specifier_safety::specifier_may_carry_credential;

use super::looks_sensitive;

/// Hex digits per group of a canonical UUID.
const UUID_GROUP_LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];
/// Canonical UUID length including its four separators.
const UUID_LENGTH: usize = 36;
/// Minimum digit count after the `h` of a localization handle.
const MINIMUM_HANDLE_DIGITS: usize = 12;
/// Shortest identifier treated as a resource name.
const MINIMUM_NAME_BYTES: usize = 2;
/// All-lowercase words shorter than this are ordinary prose, not identifiers.
const SHORT_LOWERCASE_WORD_BYTES: usize = 8;
/// Account (`user@host`) syntax, which BG3 identifiers never contain.
const ACCOUNT_MARKER: char = '@';
/// Authority marker of absolute (`https://`) and scheme-relative (`//host`)
/// URLs; a single `/` is ordinary BG3 syntax (`8d6/2`, asset paths).
const URL_AUTHORITY_MARKER: &str = "//";
/// Values that are keywords or primitive names, never resources.
const VALUE_STOPWORDS: [&str; 46] = [
    "Add", "Always", "AND", "Bool", "Boolean", "False", "IF", "Integer", "LSString", "NOT", "None",
    "NULL", "Object", "OR", "Remove", "SELF", "Source", "String", "Target", "THEN", "True",
    "Version", "and", "bool", "clear", "context", "false", "float", "guid", "int32", "int64",
    "lod", "not", "null", "off", "on", "or", "self", "source", "target", "true", "uint32",
    "uint64", "uint8", "value", "version",
];

/// Identifier tokens a BG3 value refers to, with their byte offsets: whole
/// UUIDs, GUIDSTRINGs, and handles, plus identifier-like names
/// (`UnlockSpell(Target_Spell);Darkvision` yields all three names).
pub(super) fn reference_tokens(value: &str) -> Vec<(usize, &str)> {
    if may_carry_credential(value) {
        return Vec::new();
    }
    let bytes = value.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !is_token_byte(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while cursor < bytes.len() && is_token_byte(bytes[cursor]) {
            cursor += 1;
        }
        push_run_tokens(&mut tokens, start, &value[start..cursor]);
    }
    tokens
}

/// URLs and `user@host` values are locations, never game references: their
/// `user:password` runs would look like qualified names. A value holding an
/// issued provider key (`glpat-...`, `AKIA...`) is dropped whole, because the
/// `-` split below would otherwise emit the bare key body.
fn may_carry_credential(value: &str) -> bool {
    value.contains(ACCOUNT_MARKER)
        || value.contains(URL_AUTHORITY_MARKER)
        || specifier_may_carry_credential(value)
}

const fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
}

fn push_run_tokens<'value>(tokens: &mut Vec<(usize, &'value str)>, start: usize, run: &'value str) {
    let trimmed = run.trim_start_matches(['.', ':', '-']);
    let start = start + (run.len() - trimmed.len());
    let run = trimmed.trim_end_matches(['.', ':', '-']);
    if is_uuid(run) || is_guid_string(run) || is_handle(run) {
        if !is_zero_uuid(run) {
            tokens.push((start, run));
        }
        return;
    }
    let mut offset = start;
    for part in run.split('-') {
        let part_end = part.trim_end_matches(['.', ':']);
        if is_identifier_name(part_end) {
            tokens.push((offset, part_end));
        }
        offset += part.len() + 1;
    }
}

/// `[A-Za-z_][A-Za-z0-9_.:]*` that is not a stopword or short prose word.
fn is_identifier_name(value: &str) -> bool {
    value.len() >= MINIMUM_NAME_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|byte| is_word_byte(byte) || matches!(byte, b'.' | b':'))
        && !VALUE_STOPWORDS.contains(&value)
        && !is_short_lowercase_word(value)
}

fn is_short_lowercase_word(value: &str) -> bool {
    value.len() < SHORT_LOWERCASE_WORD_BYTES && value.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// A trimmed UUID/Guid/id usable as a game-global qualified name: wholly a
/// UUID, GUIDSTRING, handle, or identifier, not the zero UUID, and not
/// credential-shaped.
pub(super) fn global_identity(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (is_resource_name(value) && !looks_sensitive(value)).then_some(value)
}

/// A usable resource name: the whole value is one BG3 identifier or one
/// identifier-like token (never a URL or sentence that merely embeds one).
pub(super) fn is_resource_name(value: &str) -> bool {
    value.len() >= MINIMUM_NAME_BYTES
        && !is_zero_uuid(value)
        && (is_uuid(value)
            || is_guid_string(value)
            || is_handle(value)
            || is_qualified_identifier(value))
}

/// `[A-Za-z_][A-Za-z0-9_.:-]*`.
fn is_qualified_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
}

/// Exactly `[A-Za-z_][A-Za-z0-9_]*_<uuid>`: a BG3 object identity.
pub(super) fn is_guid_string(value: &str) -> bool {
    guid_string_name(value).is_some()
}

/// The `Name` part of a `Name_<uuid>` GUIDSTRING.
pub(super) fn guid_string_name(value: &str) -> Option<&str> {
    let split = value.len().checked_sub(UUID_LENGTH)?;
    let (prefix, uuid) = (value.get(..split)?, value.get(split..)?);
    let name = prefix.strip_suffix('_')?;
    let valid = is_uuid(uuid)
        && name
            .bytes()
            .next()
            .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && name.bytes().all(is_word_byte);
    valid.then_some(name)
}

pub(super) fn is_zero_uuid(value: &str) -> bool {
    is_uuid(value) && value.bytes().all(|byte| matches!(byte, b'0' | b'-'))
}

fn is_uuid(value: &str) -> bool {
    value.len() == UUID_LENGTH
        && value.split('-').map(str::len).eq(UUID_GROUP_LENGTHS)
        && value
            .split('-')
            .all(|group| group.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

const fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether the value contains a word-bounded UUID, a `Name_<uuid>`
/// GUIDSTRING, or an `h…` localization handle.
pub(super) fn contains_bg3_identifier(value: &str) -> bool {
    contains_bounded_uuid(value)
        || value
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(is_handle)
}

fn contains_bounded_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    (0..bytes.len().saturating_sub(UUID_LENGTH - 1)).any(|start| {
        let end = start + UUID_LENGTH;
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]) || bytes[start - 1] == b'_';
        let after_ok = bytes.get(end).is_none_or(|byte| !is_word_byte(*byte));
        before_ok && after_ok && value.get(start..end).is_some_and(is_uuid)
    })
}

fn is_handle(token: &str) -> bool {
    let mut bytes = token.bytes();
    bytes
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(&b'h'))
        && token.len() > MINIMUM_HANDLE_DIGITS
        && bytes.all(|byte| byte.is_ascii_hexdigit() || byte.eq_ignore_ascii_case(&b'g'))
}

/// Resource names an Anubis (Lua) string literal refers to, with offsets: the
/// whole string when it is one identifier-like name (`"RaiseAlarm"`,
/// `"S_Trigger_<uuid>"`), otherwise only the BG3 identifiers it embeds.
pub(super) fn script_string_references(value: &str) -> Vec<(usize, &str)> {
    if is_script_identifier(value) {
        return vec![(0, value)];
    }
    if !contains_bg3_identifier(value) {
        return Vec::new();
    }
    reference_tokens(value)
        .into_iter()
        .filter(|(_, token)| contains_bg3_identifier(token))
        .collect()
}

fn is_script_identifier(value: &str) -> bool {
    value.len() >= MINIMUM_NAME_BYTES
        && is_qualified_identifier(value)
        && !is_zero_uuid(value)
        && !VALUE_STOPWORDS.contains(&value)
        && !is_short_lowercase_word(value)
}

/// Lua long-bracket strings (`[[...]]`) on one line, with their offsets.
pub(super) fn long_bracket_values(line: &str) -> Vec<(usize, &str)> {
    let mut output = Vec::new();
    let mut cursor = 0;
    while let Some(open) = line[cursor..].find("[[").map(|relative| cursor + relative) {
        let start = open + 2;
        let Some(close) = line[start..].find("]]").map(|relative| start + relative) else {
            break;
        };
        output.push((start, &line[start..close]));
        cursor = close + 2;
    }
    output
}
