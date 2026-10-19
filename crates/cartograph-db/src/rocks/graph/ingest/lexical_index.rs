use super::super::{
    DOCUMENT,
    lexical::{self, Forward, ForwardMetadata, LexicalStatistics, StoredPosting},
};
use super::{
    Collection, Delta, RecordKey, Result, RevisionSnapshot, StorageRequest, codec, read_budget,
};
use crate::CanonicalSearchDocument;

pub(super) fn statistics(
    previous: Option<&RevisionSnapshot<'_>>,
    request: &StorageRequest,
) -> Result<Option<LexicalStatistics>> {
    previous
        .map(|snapshot| {
            snapshot
                .graph(&mut read_budget(request)?)?
                .lexical_statistics(&mut read_budget(request)?)
        })
        .transpose()
        .map(Option::flatten)
}

impl Delta<'_, '_> {
    pub(super) fn update_lexical(&mut self, key: &RecordKey, value: Option<&[u8]>) -> Result<()> {
        self.request.check()?;
        let id = key
            .as_bytes()
            .strip_prefix(DOCUMENT)
            .ok_or_else(codec::corrupt)?;
        let forward_key = codec::key(lexical::FORWARD, id)?;
        let previous = if self.lexical_bootstrap {
            None
        } else {
            self.previous
                .map(|snapshot| {
                    snapshot.get(
                        Collection::Search,
                        &forward_key,
                        &mut read_budget(self.request)?,
                    )
                })
                .transpose()?
                .flatten()
        };
        let metadata = previous
            .as_deref()
            .map(codec::decode::<ForwardMetadata>)
            .transpose()?;
        let document = value
            .map(codec::decode::<CanonicalSearchDocument>)
            .transpose()?;
        let unchanged = metadata
            .as_ref()
            .zip(document.as_ref())
            .is_some_and(|(old, document)| old.digest == Forward::digest(document));
        let next = if unchanged {
            None
        } else {
            self.lexical_forward(document.as_ref())?
        };
        if let Some(bytes) = previous.as_deref() {
            self.delete_obsolete_postings(bytes, id, next.as_ref(), unchanged)?;
        }
        if unchanged {
            return Ok(());
        }
        if let Some(old) = &metadata {
            self.subtract_lexical_statistics(old)?;
        }
        if let Some(next) = &next {
            if !next.indexed {
                self.lexical_statistics.scan_documents = self
                    .lexical_statistics
                    .scan_documents
                    .checked_add(1)
                    .ok_or_else(codec::corrupt)?;
            }
            self.lexical_statistics.documents = self
                .lexical_statistics
                .documents
                .checked_add(1)
                .ok_or_else(codec::corrupt)?;
            for (total, length) in self
                .lexical_statistics
                .total_lengths
                .iter_mut()
                .zip(next.corpus_lengths())
            {
                *total = total.checked_add(length).ok_or_else(codec::corrupt)?;
            }
            for (term, frequencies) in &next.terms {
                self.request.check()?;
                let posting = StoredPosting {
                    lengths: next.lengths,
                    frequencies: *frequencies,
                    symbol: next.symbol,
                };
                self.lexical_point(
                    lexical::posting_key(term, id)?,
                    Some(codec::encode(&posting)?),
                )?;
            }
        }
        self.lexical_point(forward_key, next.as_ref().map(codec::encode).transpose()?)
    }

    fn delete_obsolete_postings(
        &mut self,
        bytes: &[u8],
        id: &[u8],
        next: Option<&Forward>,
        unchanged: bool,
    ) -> Result<()> {
        lexical::visit_forward_terms(bytes, |term| {
            self.request.check()?;
            if !unchanged && next.is_none_or(|next| !next.terms.contains_key(term)) {
                self.lexical_point(lexical::posting_key(term, id)?, None)?;
            }
            Ok(())
        })
    }

    fn subtract_lexical_statistics(&mut self, old: &ForwardMetadata) -> Result<()> {
        if !old.indexed {
            self.lexical_statistics.scan_documents = self
                .lexical_statistics
                .scan_documents
                .checked_sub(1)
                .ok_or_else(codec::corrupt)?;
        }
        self.lexical_statistics.documents = self
            .lexical_statistics
            .documents
            .checked_sub(1)
            .ok_or_else(codec::corrupt)?;
        for (total, length) in self
            .lexical_statistics
            .total_lengths
            .iter_mut()
            .zip(old.corpus_lengths())
        {
            *total = total.checked_sub(length).ok_or_else(codec::corrupt)?;
        }
        Ok(())
    }

    fn lexical_forward(
        &self,
        document: Option<&CanonicalSearchDocument>,
    ) -> Result<Option<Forward>> {
        // Prior vocabulary keys stream directly from a bounded input record.
        // Only the next map allocates, alongside the live table/sparse workspace.
        let maximum = self
            .lexical_working_bytes
            .min(self.policy.maximum_working_bytes / 8)
            .min(4 * 1_048_576);
        let next = document
            .map(
                |document| match Forward::build(document, maximum, &read_budget(self.request)?) {
                    Err(super::RocksError::Limit {
                        resource: "lexical_document_workspace",
                    }) => Forward::scan_only(document, &read_budget(self.request)?),
                    result => result,
                },
            )
            .transpose()?;
        Ok(next)
    }

    pub(super) fn finish_lexical(&mut self, expected_documents: u64) -> Result<()> {
        if u64::from(self.lexical_statistics.documents) != expected_documents {
            return Err(codec::corrupt());
        }
        self.lexical_statistics.version = 1;
        self.lexical_point(
            codec::key(lexical::STATS, &[])?,
            Some(codec::encode(&self.lexical_statistics)?),
        )
    }

    fn lexical_point(&mut self, key: RecordKey, value: Option<Vec<u8>>) -> Result<()> {
        self.request.check()?;
        self.report.candidate_records += 1;
        let old = self
            .previous
            .map(|snapshot| snapshot.get(Collection::Search, &key, &mut read_budget(self.request)?))
            .transpose()?
            .flatten();
        if old == value {
            self.report.reused_records += 1;
            return Ok(());
        }
        if value.is_some() {
            self.report.written_records += 1;
        } else {
            self.report.deleted_records += 1;
        }
        self.push(Collection::Search, key, value)
    }
}
