//! Credential screening for module specifiers retained as import facts.
//!
//! A specifier becomes an `imports` reference name, an import binding, and
//! (for grammar-walked imports) an `Import` symbol whose qualified name is
//! BM25 search text, so a credential embedded in it would be persisted and
//! searchable. Screening uses the domain's shared credential detector rather
//! than a local list.

use cartograph_domain::{
    is_sensitive_value_token, token_has_provider_key_shape, value_token_is_sensitive,
};

/// Return whether `specifier` could carry a credential and must be dropped.
///
/// Reject password-bearing URL/SCP user info, URL user info identified as
/// sensitive by the domain value screen, a sensitive query/fragment parameter
/// with a nonempty value, or a case-insensitive provider-key-shaped token.
/// Ordinary path words, email addresses, usernames and markup are retained.
pub(crate) fn specifier_may_carry_credential(specifier: &str) -> bool {
    spelling_may_carry_credential(specifier)
        || (specifier.contains('\\')
            && spelling_may_carry_credential(&decoded_for_screening(specifier)))
}

/// Source-literal escapes decoded only for screening (`\/`, `\\`, quotes, `\xHH`,
/// `\uHHHH`, `\u{H..}`), so an escaped credential is still recognized while the
/// retained text keeps its source spelling.
fn decoded_for_screening(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('x') => push_hex_escape(&mut decoded, &mut characters, HEX_BYTE_DIGITS),
            Some('u') if characters.peek() == Some(&'{') => {
                characters.next();
                push_hex_escape(&mut decoded, &mut characters, MAX_BRACED_HEX_DIGITS);
                if characters.peek() == Some(&'}') {
                    characters.next();
                }
            }
            Some('u') => push_hex_escape(&mut decoded, &mut characters, HEX_UNIT_DIGITS),
            Some(escaped) => decoded.push(escaped),
            None => {}
        }
    }
    decoded
}

const HEX_RADIX: u32 = 16;
const HEX_BYTE_DIGITS: usize = 2;
const HEX_UNIT_DIGITS: usize = 4;
const MAX_BRACED_HEX_DIGITS: usize = 6;

fn push_hex_escape(
    decoded: &mut String,
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    max_digits: usize,
) {
    let mut code = 0_u32;
    let mut digits = 0;
    while digits < max_digits {
        let Some(digit) = characters
            .peek()
            .and_then(|character| character.to_digit(HEX_RADIX))
        else {
            break;
        };
        characters.next();
        code = code * HEX_RADIX + digit;
        digits += 1;
    }
    if let Some(character) = char::from_u32(code).filter(|_| digits > 0) {
        decoded.push(character);
    }
}

fn spelling_may_carry_credential(specifier: &str) -> bool {
    has_credential_user_info(specifier)
        || specifier
            .split(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
            })
            .any(token_has_provider_key_shape)
        || has_sensitive_parameters(specifier)
}

/// Inspect URL authorities independently of surrounding prose or source syntax.
fn has_credential_user_info(value: &str) -> bool {
    value.match_indices("//").any(|(start, _)| {
        if value[..start].chars().next_back().is_some_and(|character| {
            character != ':'
                && !character.is_whitespace()
                && !matches!(character, '"' | '\'' | '`' | '(' | '[' | '{' | '=' | '<')
        }) {
            return false;
        }
        let authority = value[start + "//".len()..]
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(character, '/' | '?' | '#' | '"' | '\'' | '<' | '>')
            })
            .next()
            .unwrap_or_default();
        let Some((user_info, _)) = authority.rsplit_once('@') else {
            return false;
        };
        let (username, password) = user_info.split_once(':').unwrap_or((user_info, ""));
        !password.is_empty() || value_token_is_sensitive(username)
    }) || value.split_whitespace().any(scp_has_password)
}

/// SCP clone syntax can carry a password, but an ordinary email cannot.
fn scp_has_password(value: &str) -> bool {
    let Some((before_at, target)) = value.split_once('@') else {
        return false;
    };
    let user_info = before_at
        .split_once("::")
        .map_or(before_at, |(_, rest)| rest);
    scp_host_then_path(target)
        && !user_info.contains(['/', '\\'])
        && user_info
            .split_once(':')
            .is_some_and(|(_, password)| !password.is_empty())
}

/// `host:path` as in `user@host:path`: a non-empty host name followed by the
/// colon before any query, fragment or path separator (so `mailto:a@b?x=y:z`
/// is not SCP syntax).
fn scp_host_then_path(target: &str) -> bool {
    target
        .find([':', '?', '#', '/'])
        .filter(|&end| target.as_bytes()[end] == b':')
        .is_some_and(|end| {
            end > 0
                && target[..end]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        })
}

fn has_sensitive_parameters(value: &str) -> bool {
    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '{' | '}')
        })
        .any(location_has_sensitive_parameter)
}

fn location_has_sensitive_parameter(value: &str) -> bool {
    let (head, fragment) = value
        .split_once('#')
        .map_or((value, None), |(head, fragment)| (head, Some(fragment)));
    let query = head.split_once('?').map(|(_, query)| query);
    query
        .into_iter()
        .chain(fragment)
        .flat_map(|parameters| parameters.split(['&', ';']))
        .any(sensitive_parameter_has_value)
}

fn sensitive_parameter_has_value(parameter: &str) -> bool {
    parameter.split_once('=').is_some_and(|(name, value)| {
        !value.trim().is_empty() && parameter_name_is_sensitive(name.trim())
    })
}

/// Decode parameter-name escapes and separators before the shared vocabulary.
/// Unsupported encodings abstain rather than establishing a safe name.
fn parameter_name_is_sensitive(name: &str) -> bool {
    let mut normalized = Vec::new();
    if normalized.try_reserve_exact(name.len()).is_err() {
        return true;
    }
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let Some(decoded) = decode_parameter_escape(&mut bytes) else {
                return true;
            };
            decoded
        } else {
            byte
        };
        normalized.push(if byte == b'-' { b'_' } else { byte });
    }
    std::str::from_utf8(&normalized).map_or(true, is_sensitive_value_token)
}

fn decode_parameter_escape(bytes: &mut std::str::Bytes<'_>) -> Option<u8> {
    let high = char::from(bytes.next()?).to_digit(HEX_RADIX)?;
    let low = char::from(bytes.next()?).to_digit(HEX_RADIX)?;
    u8::try_from(high * HEX_RADIX + low).ok()
}

#[cfg(test)]
mod tests {
    use super::specifier_may_carry_credential;

    #[test]
    fn user_info_and_key_shaped_segments_are_rejected_but_module_paths_are_kept() {
        for unsafe_specifier in [
            "https://reader:FAKEPASSWORDxyz@example.invalid/lib.sol",
            "git::https://deploy:pw@example.invalid/repo.git",
            "deploy:pw@example.invalid:org/repo.git",
            "https://reader:hunter2::git@example.invalid/lib.sol",
            "git::https://reader:hunter2::git@example.invalid/repo.git",
            "git::deploy:pw::git@example.invalid:org/repo.git",
            "https://example.invalid/sk_live_FAKE1234567890abcdef/module.zip",
            "https://bucket.example/AKIAIOSFODNN7EXAMPLE/vpc.zip",
            "./modules/glpat-abcdefgh12/x",
            "vendor/AKIAIOSFODNN7EXAMPLE/lib.sol",
            "https://example.invalid/m.zip?token=abc",
            "https://Zx9mQ2vL8kP4rT6yW1nB3cD5@example.invalid/m.zip",
            "https://example.invalid/m.zip#access_token=abc",
            "https://example.invalid/m.zip?api_key=abc",
            "https://example.invalid/m.zip?password=abc",
            "https://example.invalid/m.zip?token=?",
            "Glpat-aaaaaaaaaaaaaaaaaaaa",
            "SK_LIVE_FAKE1234567890abcdef",
            "//reader:FAKEPASSWORDxyz@example.invalid/module",
            "https://token:@example.invalid/module",
            "https://FAKEQUERYabcdef_token:@example.invalid/module",
            "`//reader:FAKEPASSWORDxyz@example.invalid/module`",
            "https://example.invalid/module?client-secret=FAKEQUERYabcdef",
            "https://example.invalid/module?api%5Fkey=FAKEQUERYabcdef",
            "https://example.invalid/module#client-secret=FAKEQUERYabcdef",
            "https://example.invalid/module#api%5Fkey=FAKEQUERYabcdef",
            "https://example.invalid/module?token_%FF=FAKEQUERYabcdef",
            "https://example.invalid/module?api%ZZkey=FAKEQUERYabcdef",
        ] {
            assert!(
                specifier_may_carry_credential(unsafe_specifier),
                "{unsafe_specifier}"
            );
        }
        for safe_specifier in [
            "@openzeppelin/contracts/token/ERC20/ERC20.sol",
            "git@github.com:org/repo.git",
            "git::git@github.com:org/repo.git",
            "git::ssh://git@example.invalid/org/repo.git?ref=v1.2.0",
            "git::https://example.invalid/org/terraform-aws-vpc.git?ref=a1b2c3d4e5f6a1b2c3d4e5f6",
            "hashicorp/consul/aws",
            "./modules/token",
            "./regions/asia-east1",
            "contracts/AsiaToken.sol",
            "https://example.invalid/pkg@1.0/lib.sol",
            "Foundation",
            "https://admin@example.invalid/m.php",
            "https://reader:@example.invalid/m.php",
            "https://example.invalid/token/helpers.R",
            "https://example.invalid/password/module.zip",
            "https://example.invalid/Zx9mQ2vL8kP4rT6yW1nB3cD5/m.zip",
            "https://example.invalid/m.zip?token=",
            "https://example.invalid/m.zip?token=#ordinary",
            "https://example.invalid/m.zip#secret=",
            "https://example.invalid/m.zip?ref=token",
            "https://example.invalid/m.zip?ref-name=token",
            "https://example.invalid/m.zip?ref%5Fname=token",
            "https://example.invalid/m.zip?caf%C3%A9=token",
            "https://example.invalid/m.zip#description=why?token=value",
            "Contact support@example.invalid",
            "//reader@example.invalid/module",
            "//reader:@example.invalid/module",
            "path//ordinary:label@example.invalid/module",
            "See {@link User}.",
            "The secret key grants auth token access.",
        ] {
            assert!(
                !specifier_may_carry_credential(safe_specifier),
                "{safe_specifier}"
            );
        }
    }
}
