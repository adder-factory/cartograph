//! Credential screening for module specifiers retained as import facts.
//!
//! A specifier becomes an `imports` reference name, an import binding, and
//! (for grammar-walked imports) an `Import` symbol whose qualified name is
//! BM25 search text, so a credential embedded in it would be persisted and
//! searchable. Screening uses the domain's shared credential detector rather
//! than a local list.

use cartograph_domain::{token_has_provider_key_shape, value_token_is_sensitive};

/// The only URL/SCP user info a retained specifier may carry (SSH clone user).
const SSH_SOURCE_USER: &str = "git";
/// Separators after which a specifier's authority (and its user info) starts,
/// in precedence order: a URL scheme, then a Terraform forced getter without
/// one (`git::git@host:org/repo`).
const AUTHORITY_MARKERS: [&str; 2] = ["://", "::"];

/// Return whether `specifier` could carry a credential and must be dropped.
///
/// Every specifier is rejected when it carries user info other than the
/// conventional `git` SSH user, or a segment shaped like an issued provider
/// access key (`AKIA...`, `sk_live_...`, `glpat-...`). Location-shaped
/// specifiers (a URL or one with a query string) are additionally screened
/// with the declaration-value detector: any credential word, provider-key
/// prefix in any case, or high-entropy token rejects them. Plain module paths
/// keep credential words because they name directories
/// (`contracts/token/ERC20.sol`, `./regions/asia-east1`).
pub(super) fn specifier_may_carry_credential(specifier: &str) -> bool {
    has_unconventional_user_info(specifier)
        || specifier
            .split(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
            })
            .any(token_has_provider_key_shape)
        || (is_location_shaped(specifier)
            && specifier
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .filter(|token| !token.is_empty())
                .any(value_token_is_sensitive))
}

/// A URL or query-bearing specifier names a location that can embed secrets.
fn is_location_shaped(specifier: &str) -> bool {
    specifier.contains("://") || specifier.contains('?')
}

/// User info is the text between the authority start and the first `@`, when
/// it contains no `/` (an `@` inside a path or a scoped package such as
/// `@openzeppelin/contracts` is not user info). The authority starts after the
/// FIRST marker, so separators inside a password (`reader:pw::git@host`) stay
/// part of the user info.
fn has_unconventional_user_info(specifier: &str) -> bool {
    let Some((before_at, _)) = specifier.split_once('@') else {
        return false;
    };
    let user_info = AUTHORITY_MARKERS
        .iter()
        .find_map(|marker| {
            before_at
                .find(marker)
                .map(|index| &before_at[index + marker.len()..])
        })
        .unwrap_or(before_at);
    !(user_info.is_empty() || user_info.contains('/') || user_info == SSH_SOURCE_USER)
}

#[cfg(test)]
mod tests {
    use super::specifier_may_carry_credential;

    #[test]
    fn user_info_and_key_shaped_segments_are_rejected_but_module_paths_are_kept() {
        for unsafe_specifier in [
            "https://reader:FAKEPASSWORDxyz@example.invalid/lib.sol",
            "https://admin@example.invalid/m.php",
            "git::https://deploy:pw@example.invalid/repo.git",
            "deploy:pw@example.invalid:org/repo.git",
            "https://reader:hunter2::git@example.invalid/lib.sol",
            "git::https://reader:hunter2::git@example.invalid/repo.git",
            "git::deploy:pw::git@example.invalid:org/repo.git",
            "https://example.invalid/sk_live_FAKE1234567890abcdef/module.zip",
            "https://bucket.example/AKIAIOSFODNN7EXAMPLE/vpc.zip",
            "./modules/glpat-abcdefgh12/x",
            "vendor/AKIAIOSFODNN7EXAMPLE/lib.sol",
            "https://example.invalid/password/module.zip",
            "https://example.invalid/m.zip?token=abc",
            "https://example.invalid/Zx9mQ2vL8kP4rT6yW1nB3cD5/m.zip",
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
        ] {
            assert!(
                !specifier_may_carry_credential(safe_specifier),
                "{safe_specifier}"
            );
        }
    }
}
