use std::collections::BTreeMap;

use super::{
    ACTIVE_KEY, Arc, BoundColumnFamily, CURRENT_KEY, Collection, Direction, Instant, IteratorMode,
    LAST_SEQUENCE_KEY, ProjectStore, ReadBudget, ReadOptions, Result, RevisionState, RocksError,
    Snapshot, StoredRevision, WriteBatch, decode_value, identity_key, is_published, key, operation,
    published_key, put_revision, version_count_key,
};
use serde::{Deserialize, Serialize};

const CURSOR_KEY: &[u8] = b"retention/cursor";
const COMPLETED_KEY: &[u8] = b"retention/completed-v1";
pub(super) const FLOOR_KEY: &[u8] = b"retention/floor";

/// Committed progress from one bounded project-local retention transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    /// Old generation metadata removed by this invocation.
    pub generations_removed: u16,
    /// Physical fact versions inspected, including retained versions.
    pub versions_visited: u32,
    /// Obsolete fact versions and completed obsolete tombstones deleted.
    pub versions_removed: u32,
    /// Oldest generation required by this traversal's retained snapshot set.
    pub oldest_retained_sequence: u64,
    /// All work known to this bounded invocation has been traversed.
    pub complete: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    floor: u64,
    #[serde(default)]
    reservation_sequence: Option<u64>,
    collection: usize,
    resume: Option<Vec<u8>>,
    logical_key: Option<Vec<u8>>,
    kept_anchor: bool,
    obsolete_tombstone: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
struct Completed {
    last_sequence: u64,
    floor: u64,
    keep_superseded: u8,
}

impl ProjectStore {
    /// Retain current plus recent superseded revisions and collect old versions.
    ///
    /// Each logical record keeps every version required by retained revisions
    /// and at most one older published anchor for unchanged data. Failed rows
    /// are invisible and collectible. An obsolete deletion is removed only
    /// after its older values have been drained, preventing resurrection.
    /// Progress and reference counts commit with each deletion batch, so a
    /// later call resumes after interruption without rescanning the prefix.
    /// Partial traversal keeps progressing when new revisions are reserved.
    /// A changed or unknown reservation watermark starts another pass only
    /// after that traversal ends, before completion can be certified.
    /// Existing `RocksDB` read snapshots remain valid while cleanup commits.
    /// A completed traversal is reused without scanning or writing until a
    /// revision is reserved or the retention policy/progress changes.
    ///
    /// # Errors
    /// Rejects a live writer, invalid limits, exhausted read allowance, corrupt
    /// state, or native failures. No deletion commits when this call fails.
    pub fn prune(
        &self,
        keep_superseded: u8,
        maximum_generations: u16,
        maximum_versions: u32,
        budget: &mut ReadBudget,
    ) -> Result<PruneReport> {
        if keep_superseded > 32
            || !(1..=100).contains(&maximum_generations)
            || !(1..=10_000).contains(&maximum_versions)
        {
            return Err(RocksError::Limit {
                resource: "retention_policy",
            });
        }
        budget.checkpoint()?;
        self.check_control()?;
        let _serial = self.lock_mutation()?;
        let _guard = self.retention_guard()?;
        let last_sequence = self.optional_sequence(LAST_SEQUENCE_KEY)?.unwrap_or(0);
        if let Some(report) = self.completed_retention(keep_superseded, last_sequence)? {
            budget.checkpoint()?;
            self.check_control()?;
            return Ok(report);
        }
        let meta = self.meta()?;
        let mut batch = WriteBatch::default();
        // Invalidate completion in the same transaction as partial progress.
        // Returning to an earlier keep policy must still resume that cursor.
        batch.delete_cf(&meta, COMPLETED_KEY);
        let snapshot = self.inner.database.snapshot();
        let plan = self.prune_generation_metadata(
            &snapshot,
            &mut batch,
            keep_superseded,
            maximum_generations,
            budget,
        )?;
        batch.put_cf(&meta, FLOOR_KEY, plan.floor.to_be_bytes());
        let cursor = self.retention_cursor(plan.floor, last_sequence)?;
        let report = PruneReport {
            generations_removed: plan.removed,
            versions_visited: 0,
            versions_removed: 0,
            oldest_retained_sequence: cursor.floor,
            complete: false,
        };
        let mut traversal = VersionTraversal {
            cursor,
            report,
            batch,
            decrements: BTreeMap::new(),
        };
        self.prune_versions(&snapshot, &mut traversal, maximum_versions, budget)?;
        self.commit_retention(traversal, &plan, last_sequence, keep_superseded)
    }

    fn retention_cursor(&self, floor: u64, last_sequence: u64) -> Result<Cursor> {
        let cursor: Cursor = self
            .get_meta(CURSOR_KEY)?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| RocksError::Corrupt {
                    field: "retention_cursor",
                })
            })
            .transpose()?
            .unwrap_or(Cursor {
                floor,
                reservation_sequence: Some(last_sequence),
                ..Cursor::default()
            });
        if cursor.collection > Collection::ALL.len()
            || cursor.floor > floor
            || cursor
                .reservation_sequence
                .is_some_and(|sequence| sequence > last_sequence)
        {
            return Err(RocksError::Corrupt {
                field: "retention_cursor",
            });
        }
        // Preserve suffix progress even when reservations keep arriving. At
        // traversal's end, a changed or unknown starting watermark forces a
        // fresh pass so newly inserted versions behind this cursor are visited.
        Ok(cursor)
    }

    fn commit_retention(
        &self,
        traversal: VersionTraversal,
        plan: &MetadataPlan,
        last_sequence: u64,
        keep_superseded: u8,
    ) -> Result<PruneReport> {
        let meta = self.meta()?;
        let VersionTraversal {
            mut cursor,
            mut report,
            mut batch,
            decrements,
        } = traversal;
        for (sequence, removed) in decrements {
            let count = self
                .optional_sequence(&version_count_key(sequence))?
                .ok_or(RocksError::Corrupt {
                    field: "version_count",
                })?;
            let remaining = count.checked_sub(removed).ok_or(RocksError::Corrupt {
                field: "version_count",
            })?;
            if remaining == 0 && sequence < plan.floor {
                batch.delete_cf(&meta, version_count_key(sequence));
                batch.delete_cf(&meta, published_key(sequence));
            } else {
                batch.put_cf(&meta, version_count_key(sequence), remaining.to_be_bytes());
            }
        }
        if cursor.collection == Collection::ALL.len() {
            if cursor.floor < plan.floor || cursor.reservation_sequence != Some(last_sequence) {
                cursor = Cursor {
                    floor: plan.floor,
                    reservation_sequence: Some(last_sequence),
                    ..Cursor::default()
                };
                put_cursor(&mut batch, &meta, &cursor)?;
            } else {
                report.complete = !plan.more;
                batch.delete_cf(&meta, CURSOR_KEY);
            }
        } else {
            put_cursor(&mut batch, &meta, &cursor)?;
        }
        if report.complete {
            let completed = serde_json::to_vec(&Completed {
                last_sequence,
                floor: report.oldest_retained_sequence,
                keep_superseded,
            })
            .map_err(|_| operation("retention_completed"))?;
            batch.put_cf(&meta, COMPLETED_KEY, completed);
        }
        if batch.size_in_bytes() > self.inner.policy.maximum_batch_bytes {
            return Err(RocksError::Limit {
                resource: "retention_batch_bytes",
            });
        }
        self.write(&batch)?;
        Ok(report)
    }

    fn completed_retention(
        &self,
        keep_superseded: u8,
        last_sequence: u64,
    ) -> Result<Option<PruneReport>> {
        let Some(bytes) = self.get_meta(COMPLETED_KEY)? else {
            return Ok(None);
        };
        let completed: Completed =
            serde_json::from_slice(&bytes).map_err(|_| RocksError::Corrupt {
                field: "retention_completed",
            })?;
        // Every graph write requires a reservation and a live writer. The
        // caller holds both mutation and writer guards before checking this
        // watermark. Mutable feature state does not dirty graph retention.
        // Floor/cursor checks also fence maintenance by an older executable
        // that does not know about this optional completion marker.
        Ok((completed.last_sequence == last_sequence
            && completed.keep_superseded == keep_superseded
            && self.optional_sequence(FLOOR_KEY)? == Some(completed.floor)
            && self.get_meta(CURSOR_KEY)?.is_none())
        .then_some(PruneReport {
            generations_removed: 0,
            versions_visited: 0,
            versions_removed: 0,
            oldest_retained_sequence: completed.floor,
            complete: true,
        }))
    }

    fn retention_guard(&self) -> Result<super::MutexGuard<'_, Option<super::ActiveWriter>>> {
        let mut active = self.lock_writer()?;
        if active
            .as_ref()
            .is_some_and(|writer| writer.expires_at > Instant::now())
        {
            return Err(RocksError::WriterBusy);
        }
        let meta = self.meta()?;
        let mut batch = WriteBatch::default();
        if let Some(abandoned) = active.as_ref() {
            let mut revision = self.revision(abandoned.sequence)?;
            revision.state = RevisionState::Failed;
            put_revision(&mut batch, &meta, &revision)?;
            batch.delete_cf(&meta, ACTIVE_KEY);
            batch.delete_cf(&meta, super::staging::byte_count_key(revision.sequence));
            self.clear_claims(&mut batch, revision.sequence)?;
            // Commit terminalization independently, before taking the traversal
            // snapshot. That durable progress also survives a later read limit.
            self.write(&batch)?;
            *active = None;
        }
        Ok(active)
    }

    fn prune_versions(
        &self,
        snapshot: &Snapshot<'_>,
        traversal: &mut VersionTraversal,
        maximum_versions: u32,
        budget: &mut ReadBudget,
    ) -> Result<()> {
        while let Some(&collection) = Collection::ALL.get(traversal.cursor.collection) {
            let family = self.family(collection)?;
            if !self.prune_collection(snapshot, &family, traversal, maximum_versions, budget)? {
                break;
            }
            let VersionTraversal {
                cursor,
                report,
                batch,
                decrements,
            } = traversal;
            finish_tombstone(cursor, &family, batch, decrements, report)?;
            cursor.collection += 1;
            cursor.resume = None;
            cursor.logical_key = None;
            cursor.kept_anchor = false;
        }
        Ok(())
    }

    fn prune_collection(
        &self,
        snapshot: &Snapshot<'_>,
        family: &Arc<BoundColumnFamily<'_>>,
        traversal: &mut VersionTraversal,
        maximum_versions: u32,
        budget: &mut ReadBudget,
    ) -> Result<bool> {
        let meta = self.meta()?;
        let start = traversal.cursor.resume.clone().unwrap_or_default();
        let mut options = ReadOptions::default();
        options.fill_cache(false);
        for entry in snapshot.iterator_cf_opt(
            family,
            options,
            IteratorMode::From(&start, Direction::Forward),
        ) {
            let (encoded_key, value) = entry.map_err(|_| operation("retention_scan"))?;
            if traversal
                .cursor
                .resume
                .as_deref()
                .is_some_and(|resume| encoded_key.as_ref() <= resume)
            {
                continue;
            }
            if !traversal
                .can_stage_version(encoded_key.len(), self.inner.policy.maximum_batch_bytes)
            {
                return Ok(false);
            }
            traversal.visit_version(snapshot, &meta, family, &encoded_key, &value, budget)?;
            if traversal.report.versions_visited == maximum_versions {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn prune_generation_metadata(
        &self,
        snapshot: &Snapshot<'_>,
        batch: &mut WriteBatch,
        keep_superseded: u8,
        maximum_generations: u16,
        budget: &mut ReadBudget,
    ) -> Result<MetadataPlan> {
        let meta = self.meta()?;
        let prefix = b"revision/";
        let start = b"revision0";
        let mut retained = 0_u8;
        // This is also the logical retirement boundary. Increasing retention
        // later cannot resurrect generations whose versions were collected.
        let previous_floor = self.optional_sequence(FLOOR_KEY)?.unwrap_or(0);
        let mut floor = self.optional_sequence(CURRENT_KEY)?.unwrap_or(
            self.optional_sequence(LAST_SEQUENCE_KEY)?
                .unwrap_or(0)
                .saturating_add(1),
        );
        let mut removed = 0_u16;
        let mut more = false;
        for entry in snapshot.iterator_cf(&meta, IteratorMode::From(start, Direction::Reverse)) {
            let (key, bytes) = entry.map_err(|_| operation("retention_generations"))?;
            if !key.starts_with(prefix) {
                break;
            }
            budget.charge(key.len().saturating_add(bytes.len()))?;
            let revision: StoredRevision = serde_json::from_slice(&bytes)
                .map_err(|_| RocksError::Corrupt { field: "revision" })?;
            let eligible = match revision.state {
                RevisionState::Current => false,
                RevisionState::Superseded
                    if revision.sequence >= previous_floor && retained < keep_superseded =>
                {
                    retained += 1;
                    floor = floor.min(revision.sequence);
                    false
                }
                RevisionState::Superseded | RevisionState::Failed => true,
                RevisionState::Ready | RevisionState::Staging => {
                    return Err(RocksError::Corrupt {
                        field: "unowned_revision",
                    });
                }
            };
            if !eligible {
                continue;
            }
            if removed == maximum_generations {
                more = true;
                // Unknown older history may still be protected when a long
                // failed-generation backlog precedes the retained set.
                if retained < keep_superseded {
                    floor = previous_floor;
                }
                break;
            }
            batch.delete_cf(&meta, key);
            batch.delete_cf(&meta, identity_key(&revision.generation_id));
            batch.delete_cf(&meta, super::staging::byte_count_key(revision.sequence));
            if self.optional_sequence(&version_count_key(revision.sequence))? == Some(0) {
                batch.delete_cf(&meta, version_count_key(revision.sequence));
                batch.delete_cf(&meta, published_key(revision.sequence));
            }
            removed += 1;
        }
        Ok(MetadataPlan {
            floor: floor.max(previous_floor),
            removed,
            more,
        })
    }
}

struct VersionTraversal {
    cursor: Cursor,
    report: PruneReport,
    batch: WriteBatch,
    decrements: BTreeMap<u64, u64>,
}

impl VersionTraversal {
    fn can_stage_version(&self, key_bytes: usize, maximum_batch_bytes: usize) -> bool {
        // Leave room for counters, the JSON cursor (binary keys can expand
        // fourfold), and a pending tombstone. Long-key batches commit bounded
        // progress instead of repeatedly attempting oversized transactions.
        self.batch
            .size_in_bytes()
            .saturating_add(self.decrements.len().saturating_mul(96))
            .saturating_add(128 * 1_024)
            .saturating_add(key_bytes.saturating_mul(2))
            .saturating_add(256)
            <= maximum_batch_bytes
    }

    fn visit_version(
        &mut self,
        snapshot: &Snapshot<'_>,
        meta: &Arc<BoundColumnFamily<'_>>,
        family: &Arc<BoundColumnFamily<'_>>,
        encoded_key: &[u8],
        value: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<()> {
        let VersionTraversal {
            cursor,
            report,
            batch,
            decrements,
        } = self;
        budget.charge(encoded_key.len().saturating_add(value.len()))?;
        let (logical_key, sequence) = key::decode(encoded_key)?;
        if cursor.logical_key.as_deref() != Some(logical_key.as_bytes()) {
            finish_tombstone(cursor, family, batch, decrements, report)?;
            cursor.logical_key = Some(logical_key.as_bytes().to_vec());
            cursor.kept_anchor = false;
        }
        let published = is_published(snapshot, meta, sequence)?;
        if !published || (sequence < cursor.floor && cursor.kept_anchor) {
            batch.delete_cf(family, encoded_key);
            decrement(decrements, sequence)?;
            report.versions_removed += 1;
        } else if sequence == cursor.floor {
            // The oldest retained revision already has its own value.
            // No older anchor is required for this logical record.
            cursor.kept_anchor = true;
        } else if sequence < cursor.floor {
            cursor.kept_anchor = true;
            if decode_value(value)?.is_none() {
                cursor.obsolete_tombstone = Some(encoded_key.to_vec());
            }
        }
        cursor.resume = Some(encoded_key.to_vec());
        report.versions_visited += 1;
        Ok(())
    }
}

struct MetadataPlan {
    floor: u64,
    removed: u16,
    more: bool,
}

fn decrement(counts: &mut BTreeMap<u64, u64>, sequence: u64) -> Result<()> {
    let count = counts.entry(sequence).or_default();
    *count = count.checked_add(1).ok_or(RocksError::Limit {
        resource: "retention_counts",
    })?;
    Ok(())
}

fn finish_tombstone(
    cursor: &mut Cursor,
    family: &Arc<BoundColumnFamily<'_>>,
    batch: &mut WriteBatch,
    counts: &mut BTreeMap<u64, u64>,
    report: &mut PruneReport,
) -> Result<()> {
    if let Some(key) = cursor.obsolete_tombstone.take() {
        let (_, sequence) = key::decode(&key)?;
        batch.delete_cf(family, key);
        decrement(counts, sequence)?;
        report.versions_removed += 1;
    }
    Ok(())
}

fn put_cursor(
    batch: &mut WriteBatch,
    meta: &Arc<BoundColumnFamily<'_>>,
    cursor: &Cursor,
) -> Result<()> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| operation("retention_cursor"))?;
    batch.put_cf(meta, CURSOR_KEY, bytes);
    Ok(())
}
