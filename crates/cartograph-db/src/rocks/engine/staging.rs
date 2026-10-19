use std::collections::{BTreeMap, BTreeSet};

use super::{
    Arc, BoundColumnFamily, Duration, MutexGuard, ProjectStore, ReadBudget, RecordChange, Result,
    RevisionState, RevisionWriter, RocksError, Snapshot, StoredRevision, WriteBatch,
    decode_sequence, operation, read_record_from, validate_fence, version_count_key,
};

impl ProjectStore {
    /// Persist a bounded, replay-safe delta batch without exposing it to readers.
    ///
    /// Independent batches may prepare and commit concurrently. Overlapping
    /// keys take the same ordered stripe locks, so conflicting replays cannot
    /// overwrite an acknowledged row. Native atomic counters avoid lost totals
    /// when several batches commit together. Sealing waits for all staging
    /// calls to leave their shared lifecycle guard before reading final totals.
    /// Unchanged rows reuse their existing durable version.
    ///
    /// # Errors
    /// Returns a fence, lifecycle, replay, resource, or native storage failure.
    pub fn stage(
        &self,
        writer: &RevisionWriter,
        changes: &[RecordChange],
    ) -> Result<StoredRevision> {
        self.stage_controlled(writer, changes, None)
    }

    pub(in crate::rocks) fn stage_controlled(
        &self,
        writer: &RevisionWriter,
        changes: &[RecordChange],
        control: Option<&crate::rocks::StorageRequest>,
    ) -> Result<StoredRevision> {
        if let Some(control) = control {
            control.check()?;
        }
        self.validate_changes(changes)?;
        let _lifecycle = self.staging_guard()?;
        let _keys = self.lock_change_stripes(changes)?;
        validate_fence(self.lock_writer()?.as_ref(), writer)?;
        let revision = self.revision(writer.sequence)?;
        if revision.state != RevisionState::Staging {
            return Err(RocksError::Transition);
        }
        let (batch, rows, bytes) = self.prepare_batch(writer, &revision, changes, control)?;
        let meta = self.meta()?;
        let mut batch = batch;
        if rows > 0 {
            batch.merge_cf(
                &meta,
                version_count_key(writer.sequence),
                rows.to_be_bytes(),
            );
            batch.merge_cf(&meta, byte_count_key(writer.sequence), bytes.to_be_bytes());
        }
        if batch.size_in_bytes() > self.inner.policy.maximum_batch_bytes {
            return Err(RocksError::Limit {
                resource: "batch_bytes",
            });
        }
        // The shared lifecycle guard prevents replacement until this write
        // finishes. Do not hold the heartbeat mutex across native I/O: other
        // batches must be able to enter RocksDB's WAL group-commit queue.
        validate_fence(self.lock_writer()?.as_ref(), writer)?;
        if let Some(control) = control {
            control.check()?;
        }
        if !batch.is_empty() {
            self.write(&batch)?;
        }
        self.revision(writer.sequence)
    }

    fn prepare_batch(
        &self,
        writer: &RevisionWriter,
        revision: &StoredRevision,
        changes: &[RecordChange],
        control: Option<&crate::rocks::StorageRequest>,
    ) -> Result<(WriteBatch, u64, u64)> {
        let snapshot = self.inner.database.snapshot();
        let meta = self.meta()?;
        let mut budget = ReadBudget::new(1_000_000, 256 * 1_048_576, Duration::from_secs(60))?;
        if let Some(control) = control {
            budget = budget.with_control(control.clone());
        }
        let mut batch = WriteBatch::default();
        let mut rows = 0_u64;
        let mut bytes = 0_u64;
        // At most one iterator per collection remains within this snapshot and
        // finite batch. Retaining its index blocks avoids decompressing the
        // same SST index for every prior-value check. Each seek still checks
        // its exact logical key, published sequence, budget, and cancellation.
        let mut readers = BTreeMap::new();
        for change in changes {
            if let Some(control) = control {
                control.check()?;
            }
            let family = self.family(change.collection)?;
            let key = change.key.at_sequence(writer.sequence);
            let value = change.encoded_value();
            if let Some(existing) = snapshot
                .get_cf(&family, &key)
                .map_err(|_| operation("stage_replay"))?
            {
                budget.charge(key.len().saturating_add(existing.len()))?;
                if existing != value {
                    return Err(RocksError::ReplayConflict);
                }
                continue;
            }
            let reader = readers
                .entry(change.collection)
                .or_insert_with(|| snapshot.raw_iterator_cf(&family));
            let prior = read_record_from(
                &snapshot,
                reader,
                &meta,
                revision.base_sequence.unwrap_or(0),
                &change.key,
                &mut budget,
            )?;
            let claim_key = claim_key(writer.sequence, change);
            if unchanged_replay(
                &snapshot,
                &meta,
                &claim_key,
                prior.as_deref(),
                change.value.as_deref(),
                &mut budget,
            )? {
                continue;
            }
            if prior.as_deref() == change.value.as_deref() {
                // The older payload remains shared, but acknowledging a no-op
                // still claims this logical key for the generation. Otherwise
                // a conflicting overlapping call could also be acknowledged.
                batch.put_cf(&meta, claim_key, []);
                continue;
            }
            rows = rows.checked_add(1).ok_or(RocksError::Limit {
                resource: "revision_rows",
            })?;
            let row_bytes = u64::try_from(key.len().saturating_add(value.len())).map_err(|_| {
                RocksError::Limit {
                    resource: "revision_bytes",
                }
            })?;
            bytes = bytes.checked_add(row_bytes).ok_or(RocksError::Limit {
                resource: "revision_bytes",
            })?;
            batch.put_cf(&family, key, value);
        }
        Ok((batch, rows, bytes))
    }

    fn lock_change_stripes(&self, changes: &[RecordChange]) -> Result<Vec<MutexGuard<'_, ()>>> {
        let mut stripes = BTreeSet::new();
        for change in changes {
            let mut hash = blake3::Hasher::new();
            hash.update(change.collection.name().as_bytes());
            hash.update(change.key.as_bytes());
            stripes.insert(usize::from(hash.finalize().as_bytes()[0]));
        }
        stripes
            .into_iter()
            .map(|stripe| {
                self.inner.stripes[stripe]
                    .lock()
                    .map_err(|_| operation("staging_key_lock"))
            })
            .collect()
    }

    pub(super) fn clear_claims(&self, batch: &mut WriteBatch, sequence: u64) -> Result<()> {
        let prefix = claim_prefix(sequence);
        let upper = super::key::successor(prefix.clone()).ok_or(RocksError::Corrupt {
            field: "staging_claim_range",
        })?;
        batch.delete_range_cf(&self.meta()?, prefix, upper);
        Ok(())
    }
}

fn unchanged_replay(
    snapshot: &Snapshot<'_>,
    meta: &Arc<BoundColumnFamily<'_>>,
    claim_key: &[u8],
    prior: Option<&[u8]>,
    proposed: Option<&[u8]>,
    budget: &mut ReadBudget,
) -> Result<bool> {
    let Some(claim) = snapshot
        .get_cf(meta, claim_key)
        .map_err(|_| operation("stage_replay"))?
    else {
        return Ok(false);
    };
    budget.charge(claim_key.len().saturating_add(claim.len()))?;
    if !claim.is_empty() {
        return Err(RocksError::Corrupt {
            field: "staging_claim",
        });
    }
    if prior != proposed {
        return Err(RocksError::ReplayConflict);
    }
    Ok(true)
}

fn claim_prefix(sequence: u64) -> Vec<u8> {
    let mut key = b"unchanged/".to_vec();
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn claim_key(sequence: u64, change: &RecordChange) -> Vec<u8> {
    let mut key = claim_prefix(sequence);
    key.extend_from_slice(change.collection.name().as_bytes());
    key.push(b'/');
    key.extend_from_slice(change.key.as_bytes());
    key
}

pub(super) fn byte_count_key(sequence: u64) -> Vec<u8> {
    let mut key = b"staged_bytes/".to_vec();
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

pub(super) fn counter(
    snapshot: &Snapshot<'_>,
    meta: &Arc<BoundColumnFamily<'_>>,
    key: &[u8],
) -> Result<u64> {
    snapshot
        .get_cf(meta, key)
        .map_err(|_| operation("read_staging_counter"))?
        .map_or(Ok(0), |bytes| decode_sequence(&bytes))
}
