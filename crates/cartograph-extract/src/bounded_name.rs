//! Deterministic shortening for synthesized qualified and reference names.
//!
//! An extractor synthesizes names from source structure, so a single ordinary
//! construct — a long method chain, or a re-export group naming a module's whole
//! public surface — can produce a name longer than the canonical storage bound.
//! Rejecting the name discards the whole generation and costs the user the
//! entire index, so an over-long name is shortened instead: a prefix cut on a
//! character boundary, a `~` marker, and a digest of the exact original name.
//!
//! The digest keeps distinct originals distinct, and the marker keeps a
//! shortened name recognisable rather than silently passing as complete.

use cartograph_domain::ContentDigest;

/// Canonical byte bound for a stored qualified name.
pub(crate) const MAX_CANONICAL_QUALIFIED_NAME_BYTES: usize = 2_048;
/// Canonical byte bound for a stored reference name.
pub(crate) const MAX_CANONICAL_REFERENCE_NAME_BYTES: usize = 4_096;

/// Marker separating a retained prefix from the original-name digest.
const TRUNCATION_MARKER: char = '~';
/// Hexadecimal digest characters retained after the marker.
const TRUNCATION_DIGEST_CHARS: usize = 16;
/// Bytes reserved for the marker and the retained digest.
const TRUNCATION_SUFFIX_BYTES: usize = TRUNCATION_DIGEST_CHARS + 1;

/// Shorten `name` to `limit` bytes, or return `None` when it already fits.
///
/// The result is a pure function of the original name, so re-extracting an
/// unchanged file always produces an identical canonical name.
pub(crate) fn shortened_canonical_name(name: &str, limit: usize) -> Option<String> {
    if name.len() <= limit {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(name.as_bytes());
    let digest = ContentDigest::from_bytes(*hasher.finalize().as_bytes());
    let digest = digest.as_str().get(..TRUNCATION_DIGEST_CHARS)?;

    let prefix_budget = limit.saturating_sub(TRUNCATION_SUFFIX_BYTES);
    let boundary = name.floor_char_boundary(prefix_budget);
    let mut shortened = String::with_capacity(boundary + TRUNCATION_SUFFIX_BYTES);
    shortened.push_str(name.get(..boundary)?);
    shortened.push(TRUNCATION_MARKER);
    shortened.push_str(digest);
    Some(shortened)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CANONICAL_QUALIFIED_NAME_BYTES, MAX_CANONICAL_REFERENCE_NAME_BYTES,
        shortened_canonical_name,
    };

    #[test]
    fn names_within_the_bound_are_never_rewritten() {
        assert_eq!(shortened_canonical_name("alpha::beta", 2_048), None);
        let exact = "x".repeat(MAX_CANONICAL_QUALIFIED_NAME_BYTES);
        assert_eq!(
            shortened_canonical_name(&exact, MAX_CANONICAL_QUALIFIED_NAME_BYTES),
            None
        );
    }

    #[test]
    fn shortened_names_fit_the_bound_and_stay_distinct() {
        let limit = MAX_CANONICAL_QUALIFIED_NAME_BYTES;
        let first = format!("{}alpha", "long::".repeat(1_000));
        let second = format!("{}beta", "long::".repeat(1_000));
        let (Some(first), Some(second)) = (
            shortened_canonical_name(&first, limit),
            shortened_canonical_name(&second, limit),
        ) else {
            panic!("an over-long name must be shortened");
        };
        assert!(first.len() <= limit);
        assert!(second.len() <= limit);
        assert_ne!(
            first, second,
            "names sharing a prefix must not collapse together"
        );
    }

    #[test]
    fn shortening_is_deterministic_across_calls() {
        let name = "a".repeat(MAX_CANONICAL_REFERENCE_NAME_BYTES + 1);
        assert_eq!(
            shortened_canonical_name(&name, MAX_CANONICAL_REFERENCE_NAME_BYTES),
            shortened_canonical_name(&name, MAX_CANONICAL_REFERENCE_NAME_BYTES)
        );
    }

    #[test]
    fn multibyte_names_are_cut_on_a_character_boundary() {
        let name = "é".repeat(4_000);
        let Some(shortened) = shortened_canonical_name(&name, MAX_CANONICAL_QUALIFIED_NAME_BYTES)
        else {
            panic!("an over-long name must be shortened");
        };
        assert!(shortened.len() <= MAX_CANONICAL_QUALIFIED_NAME_BYTES);
        assert!(
            shortened.is_char_boundary(shortened.len()),
            "a shortened name must remain valid UTF-8"
        );
    }
}
