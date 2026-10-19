//! Reuse native index blocks within one finite batch or immutable graph view.

use rocksdb::DBRawIterator;
use std::{collections::BTreeMap, sync::Mutex};

use super::{
    Arc, BoundColumnFamily, Collection, ReadBudget, ReadOptions, RecordKey, Result,
    RevisionSnapshot, Snapshot, decode_value, is_published, key, operation,
};

#[cfg(test)]
mod tests;

/// At most four native readers borrow the exact graph view that owns them.
pub(in crate::rocks) struct PointReaders<'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    readers: Mutex<BTreeMap<Collection, DBRawIterator<'view>>>,
}

impl<'store> RevisionSnapshot<'store> {
    pub(in crate::rocks) fn point_readers(&self) -> PointReaders<'_, 'store> {
        PointReaders {
            snapshot: self,
            readers: Mutex::new(BTreeMap::new()),
        }
    }
}

impl PointReaders<'_, '_> {
    pub(in crate::rocks) fn get(
        &self,
        collection: Collection,
        key: &RecordKey,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        budget.checkpoint()?;
        let snapshot = self.snapshot;
        let family = snapshot.store.family(collection)?;
        let meta = snapshot.store.meta()?;
        let mut readers = self
            .readers
            .lock()
            .map_err(|_| operation("point_reader_lock"))?;
        // No per-key upper bound survives a seek to a different key. The shared
        // reader still checks the exact encoded prefix and charges every native
        // record it visits, including an adjacent record on a missing-key seek.
        let reader = readers
            .entry(collection)
            .or_insert_with(|| snapshot.snapshot.raw_iterator_cf(&family));
        read_record_from(
            &snapshot.snapshot,
            reader,
            &meta,
            snapshot.revision.sequence,
            key,
            budget,
        )
    }
}

pub(super) fn read_record(
    snapshot: &Snapshot<'_>,
    family: &Arc<BoundColumnFamily<'_>>,
    meta: &Arc<BoundColumnFamily<'_>>,
    sequence: u64,
    key: &RecordKey,
    budget: &mut ReadBudget,
) -> Result<Option<Vec<u8>>> {
    // Zero is the empty published base, including after failed staging work.
    if sequence == 0 {
        return Ok(None);
    }
    let mut options = ReadOptions::default();
    if let Some(upper) = key::successor(key.prefix()) {
        options.set_iterate_upper_bound(upper);
    }
    let mut iterator = snapshot.raw_iterator_cf_opt(family, options);
    read_record_from(snapshot, &mut iterator, meta, sequence, key, budget)
}

pub(super) fn read_record_from(
    snapshot: &Snapshot<'_>,
    iterator: &mut DBRawIterator<'_>,
    meta: &Arc<BoundColumnFamily<'_>>,
    sequence: u64,
    key: &RecordKey,
    budget: &mut ReadBudget,
) -> Result<Option<Vec<u8>>> {
    budget.checkpoint()?;
    if sequence == 0 {
        return Ok(None);
    }
    let prefix = key.prefix();
    iterator.seek(key.at_sequence(sequence));
    while let Some((encoded_key, value)) = iterator.item() {
        budget.charge(encoded_key.len().saturating_add(value.len()))?;
        if !encoded_key.starts_with(&prefix) {
            break;
        }
        let (_, candidate) = key::decode(encoded_key)?;
        if is_published(snapshot, meta, candidate)? {
            return Ok(decode_value(value)?.map(<[u8]>::to_vec));
        }
        iterator.next();
    }
    iterator.status().map_err(|_| operation("read_record"))?;
    Ok(None)
}
