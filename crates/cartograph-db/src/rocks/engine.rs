use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{Duration, Instant},
};

use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};
use rocksdb::{
    BlockBasedOptions, BoundColumnFamily, Cache, ColumnFamilyDescriptor, DB, DBCompressionType,
    Direction, IteratorMode, LogLevel, LruCacheOptions, MergeOperands, Options, ReadOptions,
    Snapshot, WriteBatch, WriteBufferManager, WriteOptions, checkpoint::Checkpoint,
};
use serde::{Deserialize, Serialize};

use super::{Collection, ReadBudget, RecordKey, Result, RocksError, StorePolicy, key, operation};

const FORMAT_VERSION: &[u8] = b"cartograph-rocks-v1";
const FORMAT_KEY: &[u8] = b"format";
const PROJECT_KEY: &[u8] = b"project";
const CURRENT_KEY: &[u8] = b"current";
const LAST_SEQUENCE_KEY: &[u8] = b"last_sequence";
const ACTIVE_KEY: &[u8] = b"active";
pub(super) const MAXIMUM_RECORD_BYTES: usize = 32 * 1_048_576;
const META: &str = "default";

mod lookup;
mod ownership;
pub(in crate::rocks) use lookup::PointReaders;
use lookup::{read_record, read_record_from};
mod usage;
pub use usage::NativeStorageUsage;
mod retention;
mod staging;
mod state;

#[cfg(test)]
mod tests;

pub use retention::PruneReport;
pub use state::{StateNamespace, StateTransaction};

/// Durable generation state. Only published revisions can answer graph reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionState {
    /// Bounded fact batches may still be staged.
    Staging,
    /// The complete generation was validated and sealed.
    Ready,
    /// The project current pointer selects this revision.
    Current,
    /// A newer revision replaced this complete immutable generation.
    Superseded,
    /// The operation failed; its fact versions are never visible.
    Failed,
}

/// Metadata for one immutable logical graph revision.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct StoredRevision {
    /// Stable generation identity.
    pub generation_id: GenerationId,
    /// Monotonic project-local reservation order.
    pub sequence: u64,
    /// Exact current revision against which this delta was prepared.
    pub base_sequence: Option<u64>,
    /// Complete source manifest identity supplied by the native indexer.
    pub source_revision: ContentDigest,
    /// Complete validated logical-fact digest, present after sealing.
    pub content_digest: Option<ContentDigest>,
    /// Contract interpreting the logical-fact digest.
    pub digest_version: Option<GenerationDigestVersion>,
    /// Durable lifecycle state.
    pub state: RevisionState,
    /// Number of newly persisted versions, excluding reused unchanged facts.
    pub changed_rows: u64,
    /// Encoded key/value bytes for the newly persisted fact versions.
    pub changed_bytes: u64,
    /// Reservation time, absent for records created before timestamp support.
    #[serde(default)]
    pub reserved_at: Option<String>,
    /// Publication time used by temporal graph analysis; absent in older records.
    #[serde(default)]
    pub published_at: Option<String>,
    /// Exact additional source exclusions used by this revision.
    #[serde(default, skip_serializing)]
    pub run_excludes: Vec<String>,
    /// Optional request-specific source-file size ceiling.
    #[serde(default)]
    pub maximum_file_bytes: Option<usize>,
}

/// Exact mutation authority, bound to one owner and one revision reservation.
///
/// The token carries no directory or source text. It cannot be reconstructed
/// from diagnostic metadata or reused after reopening the project database.
#[derive(Clone)]
pub struct RevisionWriter {
    generation_id: GenerationId,
    sequence: u64,
    fence: [u8; 32],
}

impl RevisionWriter {
    /// Reserved immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic reservation sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

struct ActiveWriter {
    sequence: u64,
    fence: [u8; 32],
    expires_at: Instant,
}

struct Inner {
    database: DB,
    // Fields drop in declaration order: destroy the native database before
    // explicitly unlocking, including when another process inherited the fd.
    _primary: ownership::PrimaryLock,
    identity: ContentDigest,
    mutation: RwLock<()>,
    stripes: [Mutex<()>; 256],
    writer: Mutex<Option<ActiveWriter>>,
    policy: StorePolicy,
}

/// The sole writable owner of one project's `RocksDB` directory.
///
/// Clones share the same native handle and mutation fence. Opening a second
/// primary for this project fails through an owned file lock before opening
/// `RocksDB`; the native process lock remains an additional guard. Runtime
/// adapters must route all sessions through this owner.
#[derive(Clone)]
pub struct ProjectStore {
    inner: Arc<Inner>,
    control: Option<super::StorageRequest>,
}

/// One admitted primary or secondary-index row change in a graph revision.
///
/// All indexes affected by one logical change must be staged before sealing;
/// publication makes the entire sealed revision visible atomically.
pub struct RecordChange {
    collection: Collection,
    key: RecordKey,
    value: Option<Vec<u8>>,
}

/// A finite keyset page whose cursor also covers byte-limited short pages.
pub struct RecordPage {
    /// Visible logical records in ascending key order.
    pub rows: Vec<(RecordKey, Vec<u8>)>,
    /// Exclusive cursor for another page, or `None` after complete traversal.
    pub next_after: Option<RecordKey>,
}

impl RecordChange {
    pub(super) fn admission_bytes(&self) -> Result<usize> {
        self.key
            .as_bytes()
            .len()
            .checked_mul(5)
            .and_then(|bytes| {
                bytes.checked_add(self.value.as_ref().map_or(0, Vec::len).checked_mul(3)?)
            })
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or(RocksError::Limit {
                resource: "worker_bytes",
            })
    }

    /// Stage a value. Empty values are distinct from deletions.
    #[must_use]
    pub const fn put(collection: Collection, key: RecordKey, value: Vec<u8>) -> Self {
        Self {
            collection,
            key,
            value: Some(value),
        }
    }

    /// Stage a tombstone for an existing logical record.
    #[must_use]
    pub const fn delete(collection: Collection, key: RecordKey) -> Self {
        Self {
            collection,
            key,
            value: None,
        }
    }

    fn encoded_value(&self) -> Vec<u8> {
        match &self.value {
            Some(value) => {
                let mut encoded = Vec::with_capacity(value.len().saturating_add(1));
                encoded.push(1);
                encoded.extend_from_slice(value);
                encoded
            }
            None => vec![0],
        }
    }
}

/// A consistent `RocksDB` snapshot bound to one published logical revision.
///
/// Keeping this value alive pins storage versions. A request must share one
/// finite [`ReadBudget`] across its lookups and drop this view on completion.
pub struct RevisionSnapshot<'store> {
    snapshot: Arc<Snapshot<'store>>,
    store: &'store ProjectStore,
    revision: StoredRevision,
}

impl RevisionSnapshot<'_> {
    pub(in crate::rocks) fn project_identity(&self) -> &ContentDigest {
        &self.store.inner.identity
    }
}

impl ProjectStore {
    pub(in crate::rocks) fn cleanup_graph_revision(&self, writer: &RevisionWriter) -> Result<()> {
        // An abandoned request still owns and drains its exact incomplete
        // revision. Cleanup cannot inherit the cancellation that caused it.
        let cleanup = Self {
            inner: Arc::clone(&self.inner),
            control: None,
        };
        cleanup.fail(writer)?;
        // Repeated failed publications must not accumulate an unbounded
        // terminal backlog. The exact owner drains one small independent
        // maintenance batch even when its original request was cancelled.
        let mut budget = ReadBudget::new(20_000, 64 * 1_048_576, Duration::from_secs(2))?;
        cleanup.prune(2, 4, 10_000, &mut budget).map(|_| ())
    }

    /// Open this project's private `.cartograph/data` database.
    ///
    /// Existing PostgreSQL state is neither read nor modified. A stored project
    /// identity must match the caller's canonical-root fingerprint. Interrupted
    /// staging/ready work is terminalized before admitting a new writer.
    ///
    /// # Errors
    /// Returns a redacted directory, identity, format, lock, or storage error.
    pub fn open(
        project_root: &Path,
        project_identity: &ContentDigest,
        policy: StorePolicy,
    ) -> Result<Self> {
        Self::open_controlled(project_root, project_identity, policy, None)
    }

    pub(in crate::rocks) fn open_controlled(
        project_root: &Path,
        project_identity: &ContentDigest,
        policy: StorePolicy,
        control: Option<super::StorageRequest>,
    ) -> Result<Self> {
        if let Some(control) = &control {
            control.check()?;
        }
        let policy = policy.validate()?;
        let root = project_root
            .canonicalize()
            .map_err(|_| RocksError::Directory)?;
        if !root.is_dir() {
            return Err(RocksError::Directory);
        }
        let state = root.join(".cartograph");
        private_directory(&state)?;
        let primary = ownership::lock_primary(&state)?;
        let directory = state.join("data");
        private_directory(&directory)?;
        // Refuse foreign storage before RocksDB can recover a WAL, create a
        // column family, or write its options/log files.
        ownership::admit(&directory, project_identity)?;
        ownership::validate_native(&directory, project_identity)?;
        if let Some(control) = &control {
            control.check()?;
        }
        let (options, families) = database_options(policy);
        let database = DB::open_cf_descriptors(&options, &directory, families)
            .map_err(|_| operation("open"))?;
        let store = Self {
            inner: Arc::new(Inner {
                database,
                _primary: primary,
                identity: project_identity.clone(),
                mutation: RwLock::new(()),
                stripes: std::array::from_fn(|_| Mutex::new(())),
                writer: Mutex::new(None),
                policy,
            }),
            control,
        };
        store.initialize(project_identity)?;
        Ok(Self {
            control: None,
            ..store
        })
    }

    pub(in crate::rocks) fn with_control(&self, control: super::StorageRequest) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            control: Some(control),
        }
    }

    fn check_control(&self) -> Result<()> {
        self.control
            .as_ref()
            .map_or(Ok(()), super::StorageRequest::check)
    }

    /// Reserve a generation and acquire the exact bounded writer fence.
    ///
    /// # Errors
    /// Rejects duplicate generation identities, live writers, invalid durations,
    /// sequence exhaustion, and storage failures.
    pub fn begin_revision(
        &self,
        generation_id: GenerationId,
        source_revision: ContentDigest,
        lease_duration: Duration,
    ) -> Result<RevisionWriter> {
        let _serial = self.lock_mutation()?;
        let mut active = self.lock_writer()?;
        validate_lease_duration(lease_duration)?;
        if active
            .as_ref()
            .is_some_and(|writer| writer.expires_at > Instant::now())
        {
            return Err(RocksError::WriterBusy);
        }
        let mut batch = WriteBatch::default();
        if let Some(abandoned) = active.as_ref() {
            let mut revision = self.revision(abandoned.sequence)?;
            revision.state = RevisionState::Failed;
            put_revision(&mut batch, &self.meta()?, &revision)?;
            batch.delete_cf(&self.meta()?, staging::byte_count_key(abandoned.sequence));
            self.clear_claims(&mut batch, abandoned.sequence)?;
        }
        if self.get_meta(&identity_key(&generation_id))?.is_some() {
            return Err(RocksError::ReplayConflict);
        }
        let previous = self.optional_sequence(LAST_SEQUENCE_KEY)?.unwrap_or(0);
        let sequence = previous.checked_add(1).ok_or(RocksError::Limit {
            resource: "sequence",
        })?;
        let revision = StoredRevision {
            generation_id: generation_id.clone(),
            sequence,
            base_sequence: self.optional_sequence(CURRENT_KEY)?,
            source_revision,
            content_digest: None,
            digest_version: None,
            state: RevisionState::Staging,
            changed_rows: 0,
            changed_bytes: 0,
            reserved_at: Some(jiff::Timestamp::now().to_string()),
            published_at: None,
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        };
        let meta = self.meta()?;
        put_revision(&mut batch, &meta, &revision)?;
        batch.put_cf(&meta, identity_key(&generation_id), sequence.to_be_bytes());
        batch.put_cf(&meta, LAST_SEQUENCE_KEY, sequence.to_be_bytes());
        batch.put_cf(&meta, ACTIVE_KEY, sequence.to_be_bytes());
        batch.put_cf(&meta, version_count_key(sequence), 0_u64.to_be_bytes());
        let mut fence = [0; 32];
        getrandom::fill(&mut fence).map_err(|_| operation("writer_identity"))?;
        self.write(&batch)?;
        let expires_at = lease_deadline(lease_duration)?;
        *active = Some(ActiveWriter {
            sequence,
            fence,
            expires_at,
        });
        Ok(RevisionWriter {
            generation_id,
            sequence,
            fence,
        })
    }

    /// Renew only the matching writer, never an expired or replaced token.
    ///
    /// # Errors
    /// Returns a fence, policy, or synchronization failure.
    pub fn heartbeat(&self, writer: &RevisionWriter, duration: Duration) -> Result<()> {
        validate_lease_duration(duration)?;
        let mut active = self.lock_writer()?;
        self.check_control()?;
        validate_fence(active.as_ref(), writer)?;
        // Calculate renewal after lock acquisition: waiting for another
        // lifecycle operation must not consume the newly granted lease.
        let deadline = lease_deadline(duration)?;
        if let Some(active) = active.as_mut() {
            active.expires_at = deadline;
        }
        Ok(())
    }

    /// Seal a completely validated logical generation supplied by the indexer.
    ///
    /// The storage kernel enforces lifecycle and durability. The typed graph
    /// ingestion boundary must validate facts, indexes, and the complete digest
    /// before calling this method; sealing performs no implicit graph inference.
    ///
    /// # Errors
    /// Returns a fence, lifecycle, or storage failure.
    pub fn seal(
        &self,
        writer: &RevisionWriter,
        content_digest: ContentDigest,
        digest_version: GenerationDigestVersion,
    ) -> Result<StoredRevision> {
        self.seal_with_admission(writer, content_digest, digest_version, Vec::new(), None)
    }

    pub(super) fn seal_with_admission(
        &self,
        writer: &RevisionWriter,
        content_digest: ContentDigest,
        digest_version: GenerationDigestVersion,
        run_excludes: Vec<String>,
        maximum_file_bytes: Option<usize>,
    ) -> Result<StoredRevision> {
        let _serial = self.lock_mutation()?;
        let active = self.lock_writer()?;
        validate_fence(active.as_ref(), writer)?;
        let mut revision = self.revision(writer.sequence)?;
        if revision.state != RevisionState::Staging {
            return Err(RocksError::Transition);
        }
        revision.content_digest = Some(content_digest);
        revision.digest_version = Some(digest_version);
        revision.run_excludes = run_excludes;
        revision.maximum_file_bytes = maximum_file_bytes;
        revision.state = RevisionState::Ready;
        let mut batch = WriteBatch::default();
        put_revision(&mut batch, &self.meta()?, &revision)?;
        batch.delete_cf(&self.meta()?, staging::byte_count_key(writer.sequence));
        self.clear_claims(&mut batch, writer.sequence)?;
        validate_fence(active.as_ref(), writer)?;
        self.write(&batch)?;
        Ok(revision)
    }

    /// Atomically publish the sealed graph and all its secondary indexes.
    ///
    /// This is a small synchronous WAL-backed metadata batch. All earlier
    /// staged fact batches were durable before the pointer can be committed.
    /// A changed base pointer is rejected instead of rebasing or mixing facts.
    ///
    /// # Errors
    /// Returns a fence, lifecycle, base-generation, or storage failure.
    pub fn publish(&self, writer: &RevisionWriter) -> Result<StoredRevision> {
        let _serial = self.lock_mutation()?;
        let mut active = self.lock_writer()?;
        validate_fence(active.as_ref(), writer)?;
        let mut revision = self.revision(writer.sequence)?;
        if revision.state != RevisionState::Ready {
            return Err(RocksError::Transition);
        }
        if self.optional_sequence(CURRENT_KEY)? != revision.base_sequence {
            return Err(RocksError::GenerationChanged);
        }
        let meta = self.meta()?;
        let mut batch = WriteBatch::default();
        if let Some(previous) = revision.base_sequence {
            let mut previous = self.revision(previous)?;
            if previous.state != RevisionState::Current {
                return Err(RocksError::Corrupt {
                    field: "current_revision",
                });
            }
            previous.state = RevisionState::Superseded;
            put_revision(&mut batch, &meta, &previous)?;
        }
        revision.state = RevisionState::Current;
        revision.published_at = Some(jiff::Timestamp::now().to_string());
        put_revision(&mut batch, &meta, &revision)?;
        batch.put_cf(&meta, published_key(writer.sequence), []);
        batch.put_cf(&meta, CURRENT_KEY, writer.sequence.to_be_bytes());
        batch.delete_cf(&meta, ACTIVE_KEY);
        validate_fence(active.as_ref(), writer)?;
        self.write(&batch)?;
        *active = None;
        Ok(revision)
    }

    /// Terminalize owned staging/ready work. Published generations are immutable.
    ///
    /// Expiry does not prevent owned cleanup, but a replaced exact token does.
    ///
    /// # Errors
    /// Returns a fence, lifecycle, or storage failure.
    pub fn fail(&self, writer: &RevisionWriter) -> Result<()> {
        let _serial = self.lock_mutation()?;
        let mut active = self.lock_writer()?;
        if !active.as_ref().is_some_and(|active| {
            active.sequence == writer.sequence && active.fence == writer.fence
        }) {
            return Err(RocksError::FenceLost);
        }
        let mut revision = self.revision(writer.sequence)?;
        if !matches!(
            revision.state,
            RevisionState::Staging | RevisionState::Ready
        ) {
            return Err(RocksError::Transition);
        }
        revision.state = RevisionState::Failed;
        let meta = self.meta()?;
        let mut batch = WriteBatch::default();
        put_revision(&mut batch, &meta, &revision)?;
        batch.delete_cf(&meta, ACTIVE_KEY);
        batch.delete_cf(&meta, staging::byte_count_key(writer.sequence));
        self.clear_claims(&mut batch, writer.sequence)?;
        self.write(&batch)?;
        *active = None;
        Ok(())
    }

    /// Capture a consistent current generation, optionally requiring its ID.
    ///
    /// # Errors
    /// Returns unavailable, changed-generation, or storage errors.
    pub fn current_snapshot(
        &self,
        expected: Option<&GenerationId>,
    ) -> Result<RevisionSnapshot<'_>> {
        self.check_control()?;
        let snapshot = Arc::new(self.inner.database.snapshot());
        let meta = self.meta()?;
        let sequence = snapshot
            .get_cf(&meta, CURRENT_KEY)
            .map_err(|_| operation("read_current"))?
            .ok_or(RocksError::GenerationUnavailable)?;
        let sequence = decode_sequence(&sequence)?;
        let revision = snapshot_revision(&snapshot, &meta, sequence)?;
        if revision.state != RevisionState::Current {
            return Err(RocksError::Corrupt {
                field: "current_revision",
            });
        }
        if expected.is_some_and(|expected| expected != &revision.generation_id) {
            return Err(RocksError::GenerationChanged);
        }
        Ok(RevisionSnapshot {
            snapshot,
            store: self,
            revision,
        })
    }

    /// Capture an explicitly retained, published immutable generation.
    ///
    /// # Errors
    /// Returns unavailable or storage errors for failed/unpublished identities.
    pub fn retained_snapshot(&self, generation_id: &GenerationId) -> Result<RevisionSnapshot<'_>> {
        self.check_control()?;
        let snapshot = Arc::new(self.inner.database.snapshot());
        let meta = self.meta()?;
        let sequence = snapshot
            .get_cf(&meta, identity_key(generation_id))
            .map_err(|_| operation("read_revision"))?
            .ok_or(RocksError::GenerationUnavailable)?;
        let sequence = decode_sequence(&sequence)?;
        let floor = snapshot
            .get_cf(&meta, retention::FLOOR_KEY)
            .map_err(|_| operation("read_retention_floor"))?
            .map_or(Ok(0), |bytes| decode_sequence(&bytes))?;
        if sequence < floor {
            return Err(RocksError::GenerationUnavailable);
        }
        let revision = snapshot_revision(&snapshot, &meta, sequence)?;
        if !matches!(
            revision.state,
            RevisionState::Current | RevisionState::Superseded
        ) {
            return Err(RocksError::GenerationUnavailable);
        }
        Ok(RevisionSnapshot {
            snapshot,
            store: self,
            revision,
        })
    }

    /// Create a native consistent checkpoint, including the durable WAL state.
    ///
    /// The caller owns destination admission and archive/restore policy. Copying
    /// individual live `RocksDB` files is not a supported backup operation.
    ///
    /// # Errors
    /// Returns a redacted checkpoint error if creation fails.
    pub fn checkpoint(&self, destination: &Path) -> Result<()> {
        self.check_control()?;
        Checkpoint::new(&self.inner.database)
            .map_err(|_| operation("checkpoint"))?
            .create_checkpoint(destination)
            .map_err(|_| operation("checkpoint"))?;
        ownership::stamp_checkpoint(destination, &self.inner.identity)
    }

    fn initialize(&self, identity: &ContentDigest) -> Result<()> {
        let format = self.get_meta(FORMAT_KEY)?;
        let mut batch = WriteBatch::default();
        let meta = self.meta()?;
        if let Some(format) = format {
            if format != FORMAT_VERSION {
                return Err(RocksError::Format);
            }
            if self.get_meta(PROJECT_KEY)?.as_deref() != Some(identity.as_str().as_bytes()) {
                return Err(RocksError::ProjectMismatch);
            }
        } else {
            // A marker alone never permits adoption of unknown native rows.
            for name in family_names() {
                let family = self
                    .inner
                    .database
                    .cf_handle(name)
                    .ok_or(RocksError::Format)?;
                if self
                    .inner
                    .database
                    .iterator_cf(&family, IteratorMode::Start)
                    .next()
                    .is_some()
                {
                    return Err(RocksError::Format);
                }
            }
            batch.put_cf(&meta, FORMAT_KEY, FORMAT_VERSION);
            batch.put_cf(&meta, PROJECT_KEY, identity.as_str());
        }
        if let Some(sequence) = self.optional_sequence(ACTIVE_KEY)? {
            let mut revision = self.revision(sequence)?;
            if !matches!(
                revision.state,
                RevisionState::Staging | RevisionState::Ready
            ) {
                return Err(RocksError::Corrupt {
                    field: "active_revision",
                });
            }
            revision.state = RevisionState::Failed;
            put_revision(&mut batch, &meta, &revision)?;
            batch.delete_cf(&meta, ACTIVE_KEY);
            batch.delete_cf(&meta, staging::byte_count_key(sequence));
            self.clear_claims(&mut batch, sequence)?;
        }
        self.write(&batch)
    }

    fn validate_changes(&self, changes: &[RecordChange]) -> Result<()> {
        if changes.len() > self.inner.policy.maximum_batch_rows {
            return Err(RocksError::Limit {
                resource: "batch_rows",
            });
        }
        let mut bytes = 4_096_usize;
        let mut keys = BTreeSet::new();
        for change in changes {
            let value_bytes = change.value.as_ref().map_or(0, Vec::len);
            if value_bytes > MAXIMUM_RECORD_BYTES {
                return Err(RocksError::Limit {
                    resource: "record_bytes",
                });
            }
            bytes = bytes
                .checked_add(change.key.prefix().len())
                .and_then(|sum| sum.checked_add(value_bytes))
                .and_then(|sum| sum.checked_add(32))
                .ok_or(RocksError::Limit {
                    resource: "batch_bytes",
                })?;
            if bytes > self.inner.policy.maximum_batch_bytes {
                return Err(RocksError::Limit {
                    resource: "batch_bytes",
                });
            }
            if !keys.insert((change.collection, &change.key)) {
                return Err(RocksError::ReplayConflict);
            }
        }
        Ok(())
    }

    fn family(&self, collection: Collection) -> Result<Arc<BoundColumnFamily<'_>>> {
        self.inner
            .database
            .cf_handle(collection.name())
            .ok_or(RocksError::Format)
    }

    fn meta(&self) -> Result<Arc<BoundColumnFamily<'_>>> {
        self.inner
            .database
            .cf_handle(META)
            .ok_or(RocksError::Format)
    }

    fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner
            .database
            .get_cf(&self.meta()?, key)
            .map_err(|_| operation("read_metadata"))
    }

    fn optional_sequence(&self, key: &[u8]) -> Result<Option<u64>> {
        self.get_meta(key)?
            .map(|bytes| decode_sequence(&bytes))
            .transpose()
    }

    fn revision(&self, sequence: u64) -> Result<StoredRevision> {
        let snapshot = self.inner.database.snapshot();
        let meta = self.meta()?;
        let mut revision = snapshot_revision(&snapshot, &meta, sequence)?;
        if revision.state == RevisionState::Staging {
            revision.changed_rows =
                staging::counter(&snapshot, &meta, &version_count_key(sequence))?;
            revision.changed_bytes =
                staging::counter(&snapshot, &meta, &staging::byte_count_key(sequence))?;
        }
        Ok(revision)
    }

    fn lock_writer(&self) -> Result<MutexGuard<'_, Option<ActiveWriter>>> {
        self.inner
            .writer
            .lock()
            .map_err(|_| operation("writer_lock"))
    }

    fn lock_mutation(&self) -> Result<RwLockWriteGuard<'_, ()>> {
        self.inner
            .mutation
            .write()
            .map_err(|_| operation("mutation_lock"))
    }

    fn staging_guard(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.inner
            .mutation
            .read()
            .map_err(|_| operation("mutation_lock"))
    }

    fn write(&self, batch: &WriteBatch) -> Result<()> {
        self.check_control()?;
        let mut options = WriteOptions::default();
        options.set_sync(true);
        options.disable_wal(false);
        self.inner
            .database
            .write_opt(batch, &options)
            .map_err(|_| operation("durable_write"))
    }
}

impl RevisionSnapshot<'_> {
    /// Immutable generation metadata captured with this read view.
    #[must_use]
    pub const fn revision(&self) -> &StoredRevision {
        &self.revision
    }

    /// Read one logical record from this generation, including unchanged facts.
    ///
    /// # Errors
    /// Returns a read-budget, corrupt-value, or native storage error.
    pub fn get(
        &self,
        collection: Collection,
        key: &RecordKey,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        read_record(
            &self.snapshot,
            &self.store.family(collection)?,
            &self.store.meta()?,
            self.revision.sequence,
            key,
            budget,
        )
    }

    /// Read a bounded logical-key page in ascending byte order.
    ///
    /// `after` is exclusive and must belong to the supplied prefix. Tombstones
    /// and unpublished revisions never appear. Pages stop at four MiB or the
    /// row limit; one indivisible record may exceed the page byte target.
    /// Continue with `next_after` even when a page is shorter than `limit`.
    ///
    /// # Errors
    /// Returns an input-bound, read-budget, corrupt-value, or storage error.
    pub fn scan_prefix(
        &self,
        collection: Collection,
        prefix: &[u8],
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<RecordPage> {
        validate_scan_request(prefix, after, limit)?;
        let encoded_prefix = key::escape(prefix);
        let start = after.map_or_else(
            || Some(encoded_prefix.clone()),
            |after| key::successor(after.prefix()),
        );
        let Some(start) = start else {
            return Ok(RecordPage {
                rows: Vec::new(),
                next_after: None,
            });
        };
        let mut options = ReadOptions::default();
        if let Some(upper) = key::successor(encoded_prefix.clone()) {
            options.set_iterate_upper_bound(upper);
        }
        options.fill_cache(false);
        let family = self.store.family(collection)?;
        let meta = self.store.meta()?;
        let mut rows = Vec::new();
        let mut bytes = 0_usize;
        let mut iterator = self.snapshot.raw_iterator_cf_opt(&family, options);
        budget.checkpoint()?;
        iterator.seek(start);
        while let Some((encoded_key, value)) = iterator.item() {
            budget.charge(encoded_key.len().saturating_add(value.len()))?;
            if !encoded_key.starts_with(&encoded_prefix) {
                break;
            }
            let (logical_key, sequence) = key::decode(encoded_key)?;
            if sequence > self.revision.sequence {
                budget.checkpoint()?;
                iterator.seek(logical_key.at_sequence(self.revision.sequence));
                continue;
            }
            if !is_published(&self.snapshot, &meta, sequence)? {
                budget.checkpoint()?;
                iterator.next();
                continue;
            }
            if let Some(value) = decode_value(value)? {
                bytes = bytes
                    .saturating_add(logical_key.as_bytes().len())
                    .saturating_add(value.len());
                rows.push((logical_key.clone(), value.to_vec()));
                if rows.len() == usize::from(limit) || bytes >= 4 * 1_048_576 {
                    return Ok(RecordPage {
                        rows,
                        next_after: Some(logical_key),
                    });
                }
            }
            // A published value or tombstone shadows every older version.
            // Advance past this exact terminated key, retaining binary child
            // keys that share its raw logical-byte prefix.
            let Some(next) = key::successor(logical_key.prefix()) else {
                break;
            };
            budget.checkpoint()?;
            iterator.seek(next);
        }
        iterator.status().map_err(|_| operation("scan_records"))?;
        Ok(RecordPage {
            rows,
            next_after: None,
        })
    }
}

fn validate_scan_request(prefix: &[u8], after: Option<&RecordKey>, limit: u16) -> Result<()> {
    if limit == 0
        || prefix.len() > 4_096
        || after.is_some_and(|key| !key.as_bytes().starts_with(prefix))
    {
        return Err(RocksError::Limit {
            resource: "scan_request",
        });
    }
    Ok(())
}

fn is_published(
    snapshot: &Snapshot<'_>,
    meta: &Arc<BoundColumnFamily<'_>>,
    sequence: u64,
) -> Result<bool> {
    snapshot
        .get_cf(meta, published_key(sequence))
        .map(|value| value.is_some())
        .map_err(|_| operation("read_publication"))
}

fn decode_value(bytes: &[u8]) -> Result<Option<&[u8]>> {
    match bytes.split_first() {
        Some((0, [])) => Ok(None),
        Some((1, value)) if value.len() <= MAXIMUM_RECORD_BYTES => Ok(Some(value)),
        _ => Err(RocksError::Corrupt {
            field: "record_value",
        }),
    }
}

fn revision_key(sequence: u64) -> Vec<u8> {
    let mut key = b"revision/".to_vec();
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn identity_key(generation: &GenerationId) -> Vec<u8> {
    let mut key = b"identity/".to_vec();
    key.extend_from_slice(generation.as_str().as_bytes());
    key
}

fn published_key(sequence: u64) -> Vec<u8> {
    let mut key = b"published/".to_vec();
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn version_count_key(sequence: u64) -> Vec<u8> {
    let mut key = b"versions/".to_vec();
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn decode_sequence(bytes: &[u8]) -> Result<u64> {
    bytes
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| RocksError::Corrupt { field: "sequence" })
}

fn snapshot_revision(
    snapshot: &Snapshot<'_>,
    meta: &Arc<BoundColumnFamily<'_>>,
    sequence: u64,
) -> Result<StoredRevision> {
    let bytes = snapshot
        .get_cf(meta, revision_key(sequence))
        .map_err(|_| operation("read_revision"))?
        .ok_or(RocksError::GenerationUnavailable)?;
    let revision: StoredRevision =
        serde_json::from_slice(&bytes).map_err(|_| RocksError::Corrupt { field: "revision" })?;
    if revision.sequence != sequence || sequence == 0 {
        return Err(RocksError::Corrupt {
            field: "revision_sequence",
        });
    }
    Ok(revision)
}

fn put_revision(
    batch: &mut WriteBatch,
    meta: &Arc<BoundColumnFamily<'_>>,
    revision: &StoredRevision,
) -> Result<()> {
    #[derive(Serialize)]
    struct DurableRevision<'a> {
        #[serde(flatten)]
        revision: &'a StoredRevision,
        run_excludes: &'a [String],
    }
    let bytes = serde_json::to_vec(&DurableRevision {
        revision,
        run_excludes: &revision.run_excludes,
    })
    .map_err(|_| operation("encode_revision"))?;
    batch.put_cf(meta, revision_key(revision.sequence), bytes);
    Ok(())
}

fn validate_fence(active: Option<&ActiveWriter>, writer: &RevisionWriter) -> Result<()> {
    if active.is_some_and(|active| {
        active.sequence == writer.sequence
            && active.fence == writer.fence
            && active.expires_at > Instant::now()
    }) {
        Ok(())
    } else {
        Err(RocksError::FenceLost)
    }
}

fn lease_deadline(duration: Duration) -> Result<Instant> {
    validate_lease_duration(duration)?;
    Instant::now()
        .checked_add(duration)
        .ok_or(RocksError::Limit {
            resource: "lease_duration",
        })
}

fn validate_lease_duration(duration: Duration) -> Result<()> {
    if duration.is_zero() || duration > Duration::from_secs(3_600) {
        return Err(RocksError::Limit {
            resource: "lease_duration",
        });
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|_| RocksError::Directory)?;
        }
        Ok(_) | Err(_) => return Err(RocksError::Directory),
    }
    #[cfg(unix)]
    ownership::secure_directory(path)?;
    Ok(())
}

fn database_options(policy: StorePolicy) -> (Options, Vec<ColumnFamilyDescriptor>) {
    let cache = block_cache(policy.cache_bytes);
    let buffers = WriteBufferManager::new_write_buffer_manager_with_cache(
        policy.memtable_bytes,
        true,
        cache.clone(),
    );
    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    options.set_paranoid_checks(true);
    options.set_atomic_flush(true);
    // Native group commit supports concurrent writers without pipelining.
    // RocksDB rejects pipelined writes together with atomic CF flushes.
    options.set_enable_pipelined_write(false);
    options.set_allow_concurrent_memtable_write(true);
    // Unordered writes break the native snapshot guarantees used by readers.
    options.set_unordered_write(false);
    options.set_max_background_jobs(2);
    options.set_max_open_files(256);
    options.set_max_total_wal_size(64 * 1_048_576);
    options.set_bytes_per_sync(1_048_576);
    options.set_wal_bytes_per_sync(1_048_576);
    options.set_keep_log_file_num(2);
    options.set_max_log_file_size(1_048_576);
    options.set_max_manifest_file_size(4 * 1_048_576);
    options.set_log_level(LogLevel::Warn);
    options.set_write_buffer_manager(&buffers);
    let families = family_names()
        .map(|name| {
            let mut family = Options::default();
            if name == META {
                family.set_merge_operator_associative("cartograph_u64_add_v1", merge_counter);
            }
            family.set_compression_type(DBCompressionType::Lz4);
            family.set_bottommost_compression_type(DBCompressionType::Zstd);
            family.set_level_compaction_dynamic_level_bytes(true);
            family.set_write_buffer_size(8 * 1_048_576);
            family.set_max_write_buffer_number(3);
            family.set_write_buffer_manager(&buffers);
            let mut table = BlockBasedOptions::default();
            table.set_block_cache(&cache);
            table.set_cache_index_and_filter_blocks(true);
            table.set_bloom_filter(10.0, false);
            family.set_block_based_table_factory(&table);
            ColumnFamilyDescriptor::new(name, family)
        })
        .collect();
    (options, families)
}

fn block_cache(capacity: usize) -> Cache {
    let mut options = LruCacheOptions::default();
    options.set_capacity(capacity);
    // Shards cannot share spare capacity. The automatic 64-way split leaves
    // only 1 MiB per shard at the default budget, making existing monolithic
    // SST indexes churn. Four larger shards retain parallel cache access
    // without increasing the shared cache or write-buffer budget.
    options.set_num_shard_bits(2);
    Cache::new_lru_cache_opts(&options)
}

fn family_names() -> impl Iterator<Item = &'static str> {
    std::iter::once(META)
        .chain(Collection::ALL.into_iter().map(Collection::name))
        .chain(["cache", "spill", "sessions"])
}

fn merge_counter(
    _key: &[u8],
    existing: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut total = existing.map_or(Some(0), |bytes| decode_sequence(bytes).ok())?;
    for operand in operands {
        total = total.checked_add(decode_sequence(operand).ok()?)?;
    }
    Some(total.to_be_bytes().to_vec())
}

impl std::fmt::Debug for StoredRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredRevision")
            .field("generation_id", &self.generation_id)
            .field("sequence", &self.sequence)
            .field("base_sequence", &self.base_sequence)
            .field("source_revision", &self.source_revision)
            .field("content_digest", &self.content_digest)
            .field("digest_version", &self.digest_version)
            .field("state", &self.state)
            .field("changed_rows", &self.changed_rows)
            .field("changed_bytes", &self.changed_bytes)
            .field("reserved_at", &self.reserved_at)
            .field("published_at", &self.published_at)
            .field("run_exclude_count", &self.run_excludes.len())
            .field("maximum_file_bytes", &self.maximum_file_bytes)
            .finish()
    }
}

impl Serialize for StoredRevision {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut output = serializer.serialize_struct("StoredRevision", 13)?;
        output.serialize_field("generation_id", &self.generation_id)?;
        output.serialize_field("sequence", &self.sequence)?;
        output.serialize_field("base_sequence", &self.base_sequence)?;
        output.serialize_field("source_revision", &self.source_revision)?;
        output.serialize_field("content_digest", &self.content_digest)?;
        output.serialize_field("digest_version", &self.digest_version)?;
        output.serialize_field("state", &self.state)?;
        output.serialize_field("changed_rows", &self.changed_rows)?;
        output.serialize_field("changed_bytes", &self.changed_bytes)?;
        output.serialize_field("reserved_at", &self.reserved_at)?;
        output.serialize_field("published_at", &self.published_at)?;
        output.serialize_field("run_exclude_patterns", &self.run_excludes.len())?;
        output.serialize_field("maximum_file_bytes", &self.maximum_file_bytes)?;
        output.end()
    }
}
