//! Optional, generation-scoped exact-name postings over canonical symbol IDs.

use super::{CanonicalGenerationFacts, GraphPolicy, Result, codec};

pub(super) const PREFIX: &[u8] = b"cg1/xn/";
pub(super) const SUFFIX_BYTES: usize = 48;

pub(super) fn version(facts: &CanonicalGenerationFacts, policy: GraphPolicy) -> u8 {
    u8::from(
        facts
            .symbols()
            .len()
            .checked_mul(2 * SUFFIX_BYTES)
            .is_some_and(|bytes| bytes <= policy.maximum_working_bytes),
    )
}

pub(super) fn hash(name: &str) -> [u8; 32] {
    *blake3::hash(name.to_lowercase().as_bytes()).as_bytes()
}

pub(super) fn simple(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).next().unwrap_or(name)
}

pub(super) fn suffix(name: &str, symbol: &str) -> Result<[u8; SUFFIX_BYTES]> {
    let mut key = [0; SUFFIX_BYTES];
    key[..32].copy_from_slice(&hash(name));
    key[32..].copy_from_slice(&codec::uuid(symbol)?);
    Ok(key)
}
