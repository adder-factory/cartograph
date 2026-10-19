//! Atomic, field-aware postings. Forward records make updates and deletes local.

use std::collections::BTreeMap;

use cartograph_domain::{ContentDigest, DocumentId};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeSeed, Deserializer, Error as _, IgnoredAny, MapAccess, Visitor},
};

use super::super::{Collection, ReadBudget, RecordKey, RocksError};
use super::{GraphSnapshot, Result, codec};
use crate::CanonicalSearchDocument;

pub(super) const STATS: &[u8] = b"cg1/xl/m";
pub(super) const FORWARD: &[u8] = b"cg1/xl/d/";
const POSTING: &[u8] = b"cg1/xl/p/";

/// Corpus statistics from the exact immutable graph publication.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LexicalStatistics {
    pub(super) version: u8,
    /// All documents, including documents with an empty selected field.
    pub documents: u32,
    /// Documents whose vocabulary exceeded the optional posting workspace.
    /// Any nonzero value selects the complete bounded corpus scan.
    #[serde(default)]
    pub scan_documents: u32,
    /// Weighted total lengths for all fields, declaration names, and intent.
    pub total_lengths: [u64; 3],
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Forward {
    pub(super) digest: ContentDigest,
    pub(super) lengths: [u32; 3],
    pub(super) symbol: bool,
    #[serde(default = "indexed_by_default")]
    pub(super) indexed: bool,
    pub(super) terms: BTreeMap<String, [u32; 3]>,
}

/// Read the small header without allocating the persisted vocabulary map.
#[derive(Deserialize)]
pub(super) struct ForwardMetadata {
    pub(super) digest: ContentDigest,
    lengths: [u32; 3],
    symbol: bool,
    #[serde(default = "indexed_by_default")]
    pub(super) indexed: bool,
}

impl ForwardMetadata {
    pub(super) fn corpus_lengths(&self) -> [u64; 3] {
        corpus_lengths(self.lengths, self.symbol)
    }
}

/// Visit borrowed native term keys one at a time, including under a lower policy.
/// A failed callback stops decoding and retains the original storage error.
pub(super) fn visit_forward_terms(
    bytes: &[u8],
    mut visit: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut failure = None;
    let mut guarded = |term: &str| {
        visit(term).map_err(|error| {
            failure = Some(error);
        })
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let result = decoder
        .deserialize_map(ForwardTerms(&mut guarded))
        .and_then(|()| decoder.end());
    result.map_err(|_| failure.unwrap_or_else(codec::corrupt))
}

struct ForwardTerms<'a, F>(&'a mut F);

impl<'de, F: FnMut(&str) -> std::result::Result<(), ()>> Visitor<'de> for ForwardTerms<'_, F> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native forward record")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut found = false;
        while let Some(key) = map.next_key::<&str>()? {
            if key == "terms" {
                if found {
                    return Err(A::Error::custom("duplicate vocabulary"));
                }
                found = true;
                map.next_value_seed(TermEntries(&mut *self.0))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        if !found {
            return Err(A::Error::custom("missing vocabulary"));
        }
        Ok(())
    }
}

struct TermEntries<'a, F>(&'a mut F);

impl<'de, F: FnMut(&str) -> std::result::Result<(), ()>> DeserializeSeed<'de>
    for TermEntries<'_, F>
{
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> std::result::Result<(), D::Error> {
        decoder.deserialize_map(self)
    }
}

impl<'de, F: FnMut(&str) -> std::result::Result<(), ()>> Visitor<'de> for TermEntries<'_, F> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native vocabulary")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut previous = None;
        while let Some((term, _frequencies)) = map.next_entry::<&str, [u32; 3]>()? {
            if term.is_empty()
                || term.len() > 1024
                || term.contains('\0')
                || previous.is_some_and(|old| old >= term)
            {
                return Err(A::Error::custom("invalid vocabulary"));
            }
            (self.0)(term).map_err(|()| A::Error::custom("lexical operation stopped"))?;
            previous = Some(term);
        }
        Ok(())
    }
}

/// One exact-token match, with the original weighted field statistics.
pub struct LexicalPosting {
    /// Stable document identity, in ascending order within a term.
    pub document: DocumentId,
    /// Weighted lengths of name, code, and natural text.
    pub lengths: [u32; 3],
    /// Weighted term counts in name, code, and natural text.
    pub frequencies: [u32; 3],
    /// Whether this document describes a declaration.
    pub symbol: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredPosting {
    pub(super) lengths: [u32; 3],
    pub(super) frequencies: [u32; 3],
    pub(super) symbol: bool,
}

impl Forward {
    pub(super) fn digest(document: &CanonicalSearchDocument) -> ContentDigest {
        let mut hasher = blake3::Hasher::new_derive_key("cartograph-lexical-forward-v1");
        hasher.update(&[u8::from(document.symbol_id().is_some())]);
        for field in [
            document.qualified_name(),
            document.code(),
            document.natural_text(),
        ] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field.as_bytes());
        }
        ContentDigest::from_bytes(*hasher.finalize().as_bytes())
    }

    pub(super) fn build(
        document: &CanonicalSearchDocument,
        maximum_bytes: usize,
        budget: &ReadBudget,
    ) -> Result<Self> {
        let mut terms = BTreeMap::<String, [u32; 3]>::new();
        let mut lengths = [0_u32; 3];
        let mut bytes = 0_usize;
        for (field, (text, weight)) in [
            (document.qualified_name(), 4_u32),
            (document.code(), 1),
            (document.natural_text(), 2),
        ]
        .into_iter()
        .enumerate()
        {
            crate::search::native::tokens(text, budget, |token, _| {
                lengths[field] = lengths[field].saturating_add(weight);
                if !terms.contains_key(&token) {
                    bytes = bytes
                        .checked_add(256 + token.capacity())
                        .ok_or_else(limit)?;
                    if bytes > maximum_bytes {
                        return Err(limit());
                    }
                }
                let counts = terms.entry(token).or_default();
                counts[field] = counts[field].saturating_add(weight);
                Ok(())
            })?;
        }
        Ok(Self {
            digest: Self::digest(document),
            lengths,
            symbol: document.symbol_id().is_some(),
            indexed: true,
            terms,
        })
    }

    pub(super) fn scan_only(
        document: &CanonicalSearchDocument,
        budget: &ReadBudget,
    ) -> Result<Self> {
        let mut lengths = [0_u32; 3];
        for (field, (text, weight)) in [
            (document.qualified_name(), 4_u32),
            (document.code(), 1),
            (document.natural_text(), 2),
        ]
        .into_iter()
        .enumerate()
        {
            crate::search::native::tokens(text, budget, |_, _| {
                lengths[field] = lengths[field].saturating_add(weight);
                Ok(())
            })?;
        }
        Ok(Self {
            digest: Self::digest(document),
            lengths,
            symbol: document.symbol_id().is_some(),
            indexed: false,
            terms: BTreeMap::new(),
        })
    }

    pub(super) fn corpus_lengths(&self) -> [u64; 3] {
        corpus_lengths(self.lengths, self.symbol)
    }
}

impl GraphSnapshot<'_, '_> {
    /// Inspect persisted BM25 capability. Older generations explicitly use a scan.
    /// # Errors
    /// Rejects corrupt statistics and exhausted read controls.
    pub fn lexical_statistics(&self, budget: &mut ReadBudget) -> Result<Option<LexicalStatistics>> {
        let Some(value) =
            self.snapshot
                .get(Collection::Search, &codec::key(STATS, &[])?, budget)?
        else {
            return Ok(None);
        };
        let statistics: LexicalStatistics = codec::decode(&value)?;
        if statistics.version != 1
            || u64::from(statistics.documents) != self.info.documents
            || statistics.scan_documents > statistics.documents
        {
            return Err(codec::corrupt());
        }
        Ok(Some(statistics))
    }

    /// Read bounded postings for one exact token from this pinned generation.
    /// # Errors
    /// Rejects invalid tokens, corrupt postings, foreign cursors, or read exhaustion.
    pub fn lexical_postings(
        &self,
        token: &str,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<LexicalPosting>, Option<RecordKey>)> {
        let prefix = posting_prefix(token)?;
        let page = self
            .snapshot
            .scan_prefix(Collection::Search, &prefix, after, limit, budget)?;
        let rows = page
            .rows
            .into_iter()
            .map(|(key, value)| {
                let id = codec::symbol(
                    key.as_bytes()
                        .strip_prefix(prefix.as_slice())
                        .ok_or_else(codec::corrupt)?,
                )?;
                let document = DocumentId::parse(id.as_str()).map_err(|_| codec::corrupt())?;
                let row: StoredPosting = codec::decode(&value)?;
                if row.frequencies == [0; 3]
                    || row
                        .frequencies
                        .iter()
                        .zip(row.lengths)
                        .any(|(count, length)| *count > length)
                {
                    return Err(codec::corrupt());
                }
                Ok(LexicalPosting {
                    document,
                    lengths: row.lengths,
                    frequencies: row.frequencies,
                    symbol: row.symbol,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, page.next_after))
    }
}

fn indexed_by_default() -> bool {
    true
}

fn corpus_lengths(lengths: [u32; 3], symbol: bool) -> [u64; 3] {
    [
        u64::from(lengths.into_iter().fold(0_u32, u32::saturating_add)),
        if symbol { u64::from(lengths[0]) } else { 0 },
        u64::from(lengths[2]),
    ]
}

pub(super) fn posting_key(token: &str, document: &[u8]) -> Result<RecordKey> {
    codec::key(&posting_prefix(token)?, document)
}

fn posting_prefix(token: &str) -> Result<Vec<u8>> {
    if token.is_empty() || token.len() > 1024 || token.contains('\0') {
        return Err(RocksError::InvalidInput {
            field: "lexical_token",
        });
    }
    let mut prefix = POSTING.to_vec();
    prefix.extend_from_slice(token.as_bytes());
    prefix.push(0);
    Ok(prefix)
}

fn limit() -> RocksError {
    RocksError::Limit {
        resource: "lexical_document_workspace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_vocabulary_stops_at_the_original_callback_error() {
        let mut visited = 0;
        let expected = RocksError::Limit {
            resource: "operation_deadline",
        };
        let result = visit_forward_terms(
            br#"{"terms":{"a":[1,0,0],"b":[0,1,0],"c":[0,0,1]}}"#,
            |_| {
                visited += 1;
                if visited == 2 {
                    return Err(expected);
                }
                Ok(())
            },
        );
        assert_eq!(result, Err(expected));
        assert_eq!(visited, 2);
        for invalid in [
            br#"{"terms":{"b":[1,0,0],"a":[1,0,0]}}"#.as_slice(),
            br#"{"terms":{"a":[1,0,0],"a":[1,0,0]}}"#,
            br#"{"terms":[]}"#,
            br"{}",
        ] {
            assert!(visit_forward_terms(invalid, |_| Ok(())).is_err());
        }
    }
}
