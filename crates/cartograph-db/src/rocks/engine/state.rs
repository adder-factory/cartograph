//! Mutable project evidence is independent of immutable graph publication.

use std::{collections::BTreeMap, sync::Arc};

use super::{
    Direction, GenerationId, IteratorMode, ProjectStore, ReadBudget, Result, RevisionSnapshot,
    RocksError, Snapshot, WriteBatch, decode_sequence, operation,
};

const PREFIX: &[u8] = b"state1/";
const KEY_BYTES: usize = 4096;
const VALUE_BYTES: usize = 16 * 1_048_576;

/// Typed namespaces for mutable, project-local evidence and agent state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateNamespace {
    /// Content-fenced summaries, notes, decisions, and roles.
    Artifacts,
    /// MCP session metadata.
    Sessions,
    /// Bounded tool-call traces.
    Trace,
    /// Saved tool macros.
    Macros,
    /// Source-fenced Git history and issue attribution.
    History,
    /// Model- and content-fenced embedding records.
    Embeddings,
    /// Source-fenced coverage records.
    Coverage,
    /// Content-addressed extraction cache.
    ParseCache,
    /// Generation-fenced structural and centrality results.
    Derived,
    /// Durable maintenance and readiness metadata.
    Metadata,
}

impl StateNamespace {
    const fn byte(self) -> u8 {
        match self {
            Self::Artifacts => 1,
            Self::Sessions => 2,
            Self::Trace => 3,
            Self::Macros => 4,
            Self::History => 5,
            Self::Embeddings => 6,
            Self::Coverage => 7,
            Self::ParseCache => 8,
            Self::Derived => 9,
            Self::Metadata => 10,
        }
    }
}

/// One serializable, bounded project-state transaction on a storage worker.
/// Changes become visible together through a synchronous WAL-backed write.
pub struct StateTransaction<'store> {
    store: &'store ProjectStore,
    snapshot: Arc<Snapshot<'store>>,
    changes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    bytes: usize,
    maximum_bytes: usize,
    maximum_rows: usize,
    aborted: bool,
}

impl ProjectStore {
    pub(in crate::rocks) fn state<T>(
        &self,
        expected: Option<&GenerationId>,
        maximum_write_bytes: usize,
        maximum_write_rows: usize,
        budget: &mut ReadBudget,
        operation: impl FnOnce(&mut StateTransaction<'_>, &mut ReadBudget) -> Result<T>,
    ) -> Result<T> {
        if maximum_write_bytes > self.inner.policy.maximum_batch_bytes
            || maximum_write_rows > self.inner.policy.maximum_batch_rows
        {
            return Err(RocksError::Limit {
                resource: "state_write_policy",
            });
        }
        self.check_control()?;
        let _serial = if maximum_write_rows == 0 {
            None
        } else {
            Some(self.lock_mutation()?)
        };
        let mut transaction = StateTransaction {
            store: self,
            snapshot: Arc::new(self.inner.database.snapshot()),
            changes: BTreeMap::new(),
            bytes: 0,
            maximum_bytes: maximum_write_bytes,
            maximum_rows: maximum_write_rows,
            aborted: false,
        };
        if let Some(expected) = expected {
            let current = transaction
                .current()?
                .ok_or(RocksError::GenerationUnavailable)?;
            if &current.generation_id != expected {
                return Err(RocksError::GenerationChanged);
            }
        }
        let result = operation(&mut transaction, budget)?;
        self.check_control()?;
        if !transaction.aborted && !transaction.changes.is_empty() {
            let meta = self.meta()?;
            let mut batch = WriteBatch::default();
            for (key, value) in transaction.changes {
                match value {
                    Some(value) => batch.put_cf(&meta, key, value),
                    None => batch.delete_cf(&meta, key),
                }
            }
            self.write(&batch)?;
        }
        Ok(result)
    }
}

impl<'store> StateTransaction<'store> {
    /// Discard all changes when a typed feature operation rejects its input.
    /// An aborted transaction cannot accept further mutations.
    pub(crate) fn rollback(&mut self) {
        self.changes.clear();
        self.aborted = true;
    }
    /// Read the current generation inside the transaction's publication fence.
    /// # Errors
    /// Returns storage or malformed-metadata errors.
    pub fn current(&self) -> Result<Option<super::StoredRevision>> {
        let meta = self.store.meta()?;
        self.snapshot
            .get_cf(&meta, super::CURRENT_KEY)
            .map_err(|_| operation("state_current"))?
            .map(|bytes| super::snapshot_revision(&self.snapshot, &meta, decode_sequence(&bytes)?))
            .transpose()
    }

    /// Pin graph evidence to this transaction's immutable native sequence.
    /// # Errors
    /// Returns unavailable or malformed generation errors.
    pub fn graph_snapshot(&self) -> Result<RevisionSnapshot<'store>> {
        let revision = self.current()?.ok_or(RocksError::GenerationUnavailable)?;
        if revision.state != super::RevisionState::Current {
            return Err(RocksError::Corrupt {
                field: "current_revision",
            });
        }
        Ok(RevisionSnapshot {
            snapshot: Arc::clone(&self.snapshot),
            store: self.store,
            revision,
        })
    }

    /// Page durable revision metadata in ascending sequence under this same snapshot.
    /// # Errors
    /// Rejects invalid bounds, corrupt identities, and exhausted native read allowances.
    pub fn revisions(
        &self,
        after: Option<u64>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<super::StoredRevision>, Option<u64>)> {
        if !(1..=256).contains(&limit) {
            return Err(RocksError::InvalidInput {
                field: "revision_page",
            });
        }
        let start = after.map_or_else(|| b"revision/".to_vec(), super::revision_key);
        let meta = self.store.meta()?;
        let mut output = Vec::new();
        for row in self
            .snapshot
            .iterator_cf(&meta, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, bytes) = row.map_err(|_| operation("revision_page"))?;
            if !key.starts_with(b"revision/") {
                break;
            }
            budget.charge(key.len().saturating_add(bytes.len()))?;
            let sequence =
                decode_sequence(key.get(b"revision/".len()..).ok_or(RocksError::Format)?)?;
            if after.is_some_and(|after| sequence <= after) {
                continue;
            }
            if output.len() == usize::from(limit) {
                return Ok((output, Some(sequence.saturating_sub(1))));
            }
            let revision: super::StoredRevision = serde_json::from_slice(&bytes)
                .map_err(|_| RocksError::Corrupt { field: "revision" })?;
            if revision.sequence != sequence {
                return Err(RocksError::Corrupt {
                    field: "revision_sequence",
                });
            }
            output.push(revision);
        }
        Ok((output, None))
    }

    /// Read an explicitly retained graph using this transaction's graph/state snapshot.
    /// # Errors
    /// Rejects unpublished, retired, missing, or corrupt revision metadata.
    pub fn retained_graph_snapshot(
        &self,
        generation: &GenerationId,
    ) -> Result<RevisionSnapshot<'_>> {
        let meta = self.store.meta()?;
        let bytes = self
            .snapshot
            .get_cf(&meta, super::identity_key(generation))
            .map_err(|_| operation("state_retained_identity"))?
            .ok_or(RocksError::GenerationUnavailable)?;
        let sequence = decode_sequence(&bytes)?;
        let floor = self
            .snapshot
            .get_cf(&meta, super::retention::FLOOR_KEY)
            .map_err(|_| operation("state_retained_floor"))?
            .map_or(Ok(0), |bytes| decode_sequence(&bytes))?;
        if sequence < floor {
            return Err(RocksError::GenerationUnavailable);
        }
        let revision = super::snapshot_revision(&self.snapshot, &meta, sequence)?;
        if revision.generation_id != *generation
            || !matches!(
                revision.state,
                super::RevisionState::Current | super::RevisionState::Superseded
            )
        {
            return Err(RocksError::GenerationUnavailable);
        }
        Ok(RevisionSnapshot {
            snapshot: Arc::clone(&self.snapshot),
            store: self.store,
            revision,
        })
    }

    /// Read a value, including an earlier change in this transaction.
    /// # Errors
    /// Returns key, read-budget, or storage errors.
    pub fn get(
        &self,
        namespace: StateNamespace,
        key: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        require_key(key)?;
        let key = key_bytes(namespace, key)?;
        let value = match self.changes.get(&key) {
            Some(value) => value.clone(),
            None => self
                .snapshot
                .get_cf(&self.store.meta()?, &key)
                .map_err(|_| operation("state_get"))?,
        };
        budget.charge(key.len().saturating_add(value.as_ref().map_or(0, Vec::len)))?;
        Ok(value)
    }

    /// Batch point reads under this exact snapshot, including transaction-local changes.
    /// Pinned native values are charged before allocating Rust-owned copies.
    /// # Errors
    /// Rejects excessive keys, malformed values, cancellation or exhausted read budgets.
    pub fn get_many(
        &self,
        namespace: StateNamespace,
        keys: &[Vec<u8>],
        budget: &mut ReadBudget,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() || keys.len() > 128 {
            return Err(RocksError::Limit {
                resource: "state_batch_read_keys",
            });
        }
        budget.checkpoint()?;
        let keys = keys
            .iter()
            .map(|key| {
                require_key(key)?;
                key_bytes(namespace, key)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut options = rocksdb::ReadOptions::default();
        options.set_snapshot(&self.snapshot);
        options.set_value_size_soft_limit(8 * 1_048_576);
        let meta = self.store.meta()?;
        let rows = self
            .store
            .inner
            .database
            .batched_multi_get_pinned_cf_opt(&meta, &keys, false, &options);
        let mut output = Vec::with_capacity(keys.len());
        let mut retained = 0_usize;
        for (key, value) in keys.into_iter().zip(rows) {
            let value = value.map_err(|_| operation("state_multi_get"))?;
            budget.charge(
                key.len()
                    .saturating_add(value.as_ref().map_or(0, |value| value.len())),
            )?;
            let local = self.changes.get(&key);
            let bytes = local.map_or_else(
                || value.as_ref().map_or(0, |value| value.len()),
                |value| value.as_ref().map_or(0, Vec::len),
            );
            retained = retained.checked_add(bytes).ok_or(RocksError::Limit {
                resource: "state_batch_read_bytes",
            })?;
            if retained > 8 * 1_048_576 {
                return Err(RocksError::Limit {
                    resource: "state_batch_read_bytes",
                });
            }
            let value = if let Some(value) = local {
                budget.charge(value.as_ref().map_or(0, Vec::len))?;
                value.clone()
            } else {
                value.map(|value| value.to_vec())
            };
            output.push(value);
        }
        Ok(output)
    }

    /// Scan one namespace in key order, respecting the same physical read budget.
    /// The cursor is exclusive and belongs to this namespace.
    /// # Errors
    /// Returns key, row, byte, deadline, or storage errors.
    pub fn scan(
        &self,
        namespace: StateNamespace,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<super::RecordPage> {
        if limit == 0 || limit > 2000 {
            return Err(RocksError::Limit {
                resource: "state_page_rows",
            });
        }
        // Reads and writes use separate phases. This avoids an unbounded merge
        // buffer and makes every page correspond to this transaction snapshot.
        if !self.changes.is_empty() {
            return Err(RocksError::Transition);
        }
        let prefix = key_bytes(namespace, prefix)?;
        let start = match after {
            Some(after) => key_bytes(namespace, after)?,
            None => prefix.clone(),
        };
        if !start.starts_with(&prefix) {
            return Err(RocksError::Transition);
        }
        let meta = self.store.meta()?;
        let mut options = super::ReadOptions::default();
        options.set_iterate_lower_bound(start.clone());
        if let Some(end) = super::key::successor(prefix.clone()) {
            options.set_iterate_upper_bound(end);
        }
        let iterator = self.snapshot.iterator_cf_opt(
            &meta,
            options,
            IteratorMode::From(&start, Direction::Forward),
        );
        let mut rows = Vec::new();
        let mut next_after = None;
        let mut page_bytes = 0_usize;
        for row in iterator {
            let (key, value) = row.map_err(|_| operation("state_scan"))?;
            budget.charge(key.len().saturating_add(value.len()))?;
            if !key.starts_with(&prefix) {
                break;
            }
            if after.is_some() && key.as_ref() == start {
                continue;
            }
            if rows.len() == usize::from(limit) || page_bytes >= 4 * 1_048_576 {
                next_after = rows
                    .last()
                    .map(|(key, _): &(super::RecordKey, Vec<u8>)| key.clone());
                break;
            }
            let logical = key.get(PREFIX.len() + 1..).ok_or(RocksError::Format)?;
            page_bytes = page_bytes
                .saturating_add(key.len())
                .saturating_add(value.len());
            rows.push((super::RecordKey::new(logical.to_vec())?, value.to_vec()));
        }
        Ok(super::RecordPage { rows, next_after })
    }

    /// Replace a bounded value in this transaction's atomic write batch.
    /// # Errors
    /// Returns key, value, row, or batch-byte bounds errors.
    pub fn put(&mut self, namespace: StateNamespace, key: &[u8], value: Vec<u8>) -> Result<()> {
        if value.len() > VALUE_BYTES {
            return Err(RocksError::Limit {
                resource: "state_value_bytes",
            });
        }
        self.change(namespace, key, Some(value))
    }

    /// Delete a bounded value atomically with the other transaction changes.
    /// # Errors
    /// Returns key, row, or batch-byte bounds errors.
    pub fn delete(&mut self, namespace: StateNamespace, key: &[u8]) -> Result<()> {
        self.change(namespace, key, None)
    }

    fn change(
        &mut self,
        namespace: StateNamespace,
        key: &[u8],
        value: Option<Vec<u8>>,
    ) -> Result<()> {
        self.store.check_control()?;
        if self.aborted {
            return Err(RocksError::Transition);
        }
        require_key(key)?;
        let key = key_bytes(namespace, key)?;
        let bytes = key
            .len()
            .saturating_add(value.as_ref().map_or(0, Vec::len))
            .saturating_add(32);
        self.bytes = self.bytes.checked_add(bytes).ok_or(RocksError::Limit {
            resource: "state_write_bytes",
        })?;
        // Replacements are charged too: transient allocations count toward the
        // request's admitted bound, even if the final batch is smaller.
        if self.bytes > self.maximum_bytes
            || (self.changes.len() >= self.maximum_rows && !self.changes.contains_key(&key))
        {
            return Err(RocksError::Limit {
                resource: "state_write_bytes_or_rows",
            });
        }
        self.changes.insert(key, value);
        Ok(())
    }
}

fn require_key(key: &[u8]) -> Result<()> {
    if key.is_empty() {
        Err(RocksError::Limit {
            resource: "state_key_bytes",
        })
    } else {
        Ok(())
    }
}

fn key_bytes(namespace: StateNamespace, key: &[u8]) -> Result<Vec<u8>> {
    if key.len() > KEY_BYTES {
        return Err(RocksError::Limit {
            resource: "state_key_bytes",
        });
    }
    let mut encoded = Vec::with_capacity(PREFIX.len() + 1 + key.len());
    encoded.extend_from_slice(PREFIX);
    encoded.push(namespace.byte());
    encoded.extend_from_slice(key);
    Ok(encoded)
}
