//! Atomic visibility for larger typed auxiliary snapshots on the same state owner.

use super::{decode, encode, now};
use crate::{
    NativeDatabase, StorageError,
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::{ContentDigest, GenerationId};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, time::Duration};

const ROWS_PER_PART: usize = 256;
const PART_BYTES: u32 = 4 * 1_048_576;
const TIMEOUT: Duration = Duration::from_secs(300);
// One published head must fit a typed reader's 1 GiB physical allowance,
// including page keys, the manifest, and other snapshot metadata.
const MAXIMUM_DATASET_BYTES: u64 = 512 * 1_048_576;

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    token: String,
    pub(crate) generation: Option<GenerationId>,
    pub(crate) metadata: serde_json::Value,
    rows: u64,
    bytes: u64,
    parts: u32,
    digest: ContentDigest,
    created: String,
}

/// Exact metadata read set captured while computing a derived dataset.
#[derive(Default)]
pub(crate) struct CommitGuard(pub(crate) Vec<(&'static [u8], Option<Vec<u8>>)>);

impl CommitGuard {
    pub(crate) fn capture(
        transaction: &StateTransaction<'_>,
        keys: &[&'static [u8]],
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        if keys.len() > 8 {
            return Err(RocksError::InvalidInput {
                field: "dataset_guard",
            });
        }
        let mut entries = Vec::new();
        for &key in keys {
            let bytes = transaction.get(StateNamespace::Metadata, key, budget)?;
            if bytes.as_ref().is_some_and(|bytes| bytes.len() > 4096) {
                return Err(corrupt());
            }
            entries.push((key, bytes));
        }
        Ok(Self(entries))
    }
    fn check(
        &self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        for (key, expected) in &self.0 {
            if transaction.get(StateNamespace::Metadata, key, budget)? != *expected {
                return Err(RocksError::GenerationChanged);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Prepared {
    name: String,
    manifest: Manifest,
    reclaimed: u32,
}

/// Committed cleanup progress for obsolete auxiliary snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeAuxiliaryPruneReport {
    /// Fully collected old or interrupted snapshots.
    pub snapshots_removed: u32,
    /// Payload pages deleted in this committed batch.
    pub pages_removed: u32,
    /// More eligible work remains beyond the independent batch limits.
    pub incomplete: bool,
    /// Independently committed cold-orphan and demand-queue cleanup.
    pub derived: Option<super::retention::NativeDerivedPruneReport>,
    /// Stable native error when auxiliary pages could not be collected.
    pub pages_error: Option<String>,
    /// Stable native error when cold-orphan collection could not complete.
    pub derived_error: Option<String>,
}

impl NativeAuxiliaryPruneReport {
    /// Whether either independently committed maintenance phase failed.
    #[must_use]
    pub const fn maintenance_failed(&self) -> bool {
        self.pages_error.is_some() || self.derived_error.is_some()
    }
}

impl NativeDatabase {
    /// Prepare bounded pages and atomically publish one auxiliary snapshot.
    /// A concurrent replacement fails its pointer comparison; partial pages stay invisible.
    pub(crate) async fn replace_dataset<T, I, M>(
        &self,
        namespace: StateNamespace,
        name: &str,
        expected: Option<GenerationId>,
        metadata: &M,
        rows: I,
    ) -> Result<(), StorageError>
    where
        T: Serialize + Send + 'static,
        I: Iterator<Item = T> + Send,
        M: Serialize + Sync,
    {
        self.replace_dataset_guarded(
            namespace,
            name,
            expected,
            metadata,
            rows,
            CommitGuard::default(),
        )
        .await
    }

    pub(crate) async fn replace_dataset_guarded<T, I, M>(
        &self,
        namespace: StateNamespace,
        name: &str,
        expected: Option<GenerationId>,
        metadata: &M,
        rows: I,
        guard: CommitGuard,
    ) -> Result<(), StorageError>
    where
        T: Serialize + Send + 'static,
        I: Iterator<Item = T> + Send,
        M: Serialize + Sync,
    {
        validate_name(name)?;
        let metadata = encode(metadata).map_err(super::storage_error)?;
        if metadata.len() > 1_048_576 {
            return Err(StorageError::InvalidInput {
                field: "dataset_metadata_bytes",
            });
        }
        let metadata =
            serde_json::from_slice(&metadata).map_err(|_| StorageError::InvalidInput {
                field: "dataset_metadata",
            })?;
        Self::with_preparation_deadline(
            TIMEOUT,
            self.prepare_dataset(namespace, name.to_owned(), expected, metadata, rows, guard),
        )
        .await
    }

    async fn prepare_dataset<T, I>(
        &self,
        namespace: StateNamespace,
        name: String,
        expected: Option<GenerationId>,
        metadata: serde_json::Value,
        mut rows: I,
        guard: CommitGuard,
    ) -> Result<(), StorageError>
    where
        T: Serialize + Send + 'static,
        I: Iterator<Item = T> + Send,
    {
        let pointer = head_key(&name);
        let before = self
            .state(None, 0, 0, move |transaction, budget| {
                transaction.get(namespace, &pointer, budget)
            })
            .await?;
        let mut manifest = Manifest {
            token: token()?,
            generation: expected.clone(),
            metadata,
            rows: 0,
            bytes: 0,
            parts: 0,
            digest: ContentDigest::from_bytes([0; 32]),
            created: now(),
        };
        let mut digest = blake3::Hasher::new();
        self.write_preparation(namespace, &name, &manifest, None)
            .await?;
        loop {
            let part = rows.by_ref().take(ROWS_PER_PART).collect::<Vec<_>>();
            if part.is_empty() {
                break;
            }
            let count = part.len();
            let encoded = self
                .state(None, 0, 0, move |_, _| {
                    let mut encoded = BoundedWriter(Vec::new());
                    serde_json::to_writer(&mut encoded, &part).map_err(|_| RocksError::Limit {
                        resource: "dataset_part_bytes",
                    })?;
                    Ok(encoded.0)
                })
                .await?;
            manifest.rows =
                manifest
                    .rows
                    .checked_add(count as u64)
                    .ok_or(StorageError::InvalidInput {
                        field: "dataset_rows",
                    })?;
            manifest.bytes = manifest.bytes.checked_add(encoded.len() as u64).ok_or(
                StorageError::InvalidInput {
                    field: "dataset_bytes",
                },
            )?;
            if manifest.rows > 3_000_000 || manifest.bytes > MAXIMUM_DATASET_BYTES {
                return Err(StorageError::InvalidInput {
                    field: "dataset_capacity",
                });
            }
            digest.update(&encoded);
            let part = manifest.parts;
            manifest.parts = manifest
                .parts
                .checked_add(1)
                .ok_or(StorageError::InvalidInput {
                    field: "dataset_parts",
                })?;
            self.write_preparation(namespace, &name, &manifest, Some((part, encoded)))
                .await?;
        }
        manifest.digest = ContentDigest::from_bytes(*digest.finalize().as_bytes());
        self.state(
            expected,
            PART_BYTES + 1_048_576,
            3,
            move |transaction, budget| {
                if transaction.get(namespace, &head_key(&name), budget)? != before {
                    return Err(RocksError::GenerationChanged);
                }
                guard.check(transaction, budget)?;
                if let Some(epoch) = input_epoch(namespace, &name) {
                    crate::artifacts::native::bump_epoch(transaction, budget, epoch)?;
                }
                transaction.put(
                    namespace,
                    &prepared_key(&name, &manifest.token),
                    encode(&Prepared {
                        name: name.clone(),
                        manifest: manifest.clone(),
                        reclaimed: 0,
                    })?,
                )?;
                transaction.put(namespace, &head_key(&name), encode(&manifest)?)
            },
        )
        .await
    }

    async fn write_preparation(
        &self,
        namespace: StateNamespace,
        name: &str,
        manifest: &Manifest,
        part: Option<(u32, Vec<u8>)>,
    ) -> Result<(), StorageError> {
        let marker = prepared_key(name, &manifest.token);
        let encoded = encode(&Prepared {
            name: name.to_owned(),
            manifest: manifest.clone(),
            reclaimed: 0,
        })
        .map_err(super::storage_error)?;
        let key = part
            .as_ref()
            .map(|(part, _)| part_key(name, &manifest.token, *part));
        self.state(None, PART_BYTES + 1_048_576, 2, move |transaction, _| {
            if let Some((_, bytes)) = part {
                transaction.put(namespace, key.as_deref().ok_or(corrupt())?, bytes)?;
            }
            transaction.put(namespace, &marker, encoded)
        })
        .await
    }

    pub(crate) async fn clear_dataset(
        &self,
        namespace: StateNamespace,
        name: &str,
    ) -> Result<bool, StorageError> {
        validate_name(name)?;
        let key = head_key(name);
        let epoch = input_epoch(namespace, name);
        self.state(None, 4096, 2, move |transaction, budget| {
            let present = transaction.get(namespace, &key, budget)?.is_some();
            if present && let Some(epoch) = epoch {
                crate::artifacts::native::bump_epoch(transaction, budget, epoch)?;
            }
            transaction.delete(namespace, &key)?;
            Ok(present)
        })
        .await
    }

    /// Collect old auxiliary pages in one bounded, resumable transaction.
    /// Live heads stay protected; incomplete preparation has a ten minute grace.
    /// # Errors
    /// Rejects corrupt manifests or exhausted physical reads without partial deletion.
    pub async fn prune_auxiliary_snapshots(
        &self,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        let request = crate::native_database::request_for(std::time::Duration::from_secs(30))
            .map_err(super::storage_error)?;
        self.prune_auxiliary_snapshots_with_request(&request).await
    }

    /// Collect one auxiliary batch under the original request's deadline and cancellation.
    /// # Errors
    /// Rejects stopped requests before worker admission and preserves atomic rollback.
    pub async fn prune_auxiliary_snapshots_with_request(
        &self,
        request: &crate::rocks::StorageRequest,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        let pages = self.prune_auxiliary_pages_with_request(request).await;
        let derived = self.prune_derived_state_with_request(request).await;
        let mut report = match pages {
            Ok(report) => report,
            Err(error) => {
                if derived.is_err() {
                    return Err(error);
                }
                NativeAuxiliaryPruneReport {
                    incomplete: true,
                    pages_error: Some(error.to_string()),
                    ..NativeAuxiliaryPruneReport::default()
                }
            }
        };
        match derived {
            Ok(derived) => {
                report.incomplete |= derived.incomplete;
                report.derived = Some(derived);
            }
            Err(error) => {
                report.incomplete = true;
                report.derived_error = Some(error.to_string());
            }
        }
        Ok(report)
    }

    async fn prune_auxiliary_pages_with_request(
        &self,
        request: &crate::rocks::StorageRequest,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        self.state_with_request(
            None,
            16 * 1_048_576,
            10_000,
            request,
            |transaction, budget| {
                let mut plan = AuxiliaryGc::default();
                let timestamp = now().parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
                for namespace in [
                    StateNamespace::History,
                    StateNamespace::Coverage,
                    StateNamespace::Derived,
                    StateNamespace::Artifacts,
                ] {
                    if plan.at_capacity() {
                        plan.report.incomplete = true;
                        break;
                    }
                    plan.scan_namespace(transaction, budget, namespace, timestamp)?;
                }
                apply_gc_cursors(transaction, plan.cursors)?;
                apply_gc_plans(transaction, plan.plans)?;
                Ok(plan.report)
            },
        )
        .await
    }
}

#[derive(Default)]
struct AuxiliaryGc {
    plans: Vec<(StateNamespace, Vec<u8>, u32, Prepared)>,
    cursors: Vec<(String, Option<Vec<u8>>)>,
    report: NativeAuxiliaryPruneReport,
    rows: u32,
}

impl AuxiliaryGc {
    fn at_capacity(&self) -> bool {
        self.plans.len() >= 4 || self.rows >= 9990
    }

    fn scan_namespace(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        namespace: StateNamespace,
        timestamp: jiff::Timestamp,
    ) -> Result<(), RocksError> {
        let cursor_key = format!("gc/dataset/{namespace:?}");
        let after = transaction.get(StateNamespace::Metadata, cursor_key.as_bytes(), budget)?;
        let page = transaction.scan(namespace, b"dataset/s/", after.as_deref(), 64, budget)?;
        self.report.incomplete |= page.next_after.is_some();
        let mut visited = None;
        let mut exhausted = true;
        for (key, bytes) in page.rows {
            if self.at_capacity() {
                self.report.incomplete = true;
                exhausted = false;
                break;
            }
            visited = Some(key.as_bytes().to_vec());
            self.consider_prepared(
                transaction,
                budget,
                namespace,
                key.as_bytes(),
                &bytes,
                timestamp,
            )?;
        }
        let next = if page.next_after.is_some() || !exhausted {
            visited
        } else {
            None
        };
        self.cursors.push((cursor_key, next));
        Ok(())
    }

    fn consider_prepared(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        namespace: StateNamespace,
        key: &[u8],
        bytes: &[u8],
        timestamp: jiff::Timestamp,
    ) -> Result<(), RocksError> {
        let mut prepared: Prepared = super::decode(bytes)?;
        validate_manifest(&prepared.manifest)?;
        let created = prepared
            .manifest
            .created
            .parse::<jiff::Timestamp>()
            .map_err(|_| corrupt())?;
        if timestamp.as_second().saturating_sub(created.as_second()) < 600 {
            return Ok(());
        }
        validate_name(&prepared.name).map_err(|_| corrupt())?;
        if key != prepared_key(&prepared.name, &prepared.manifest.token)
            || prepared.manifest.parts > 12_000
            || prepared.reclaimed > prepared.manifest.parts
        {
            return Err(corrupt());
        }
        if manifest(transaction, namespace, &prepared.name, budget)?
            .is_some_and(|head| head.token == prepared.manifest.token)
        {
            return Ok(());
        }
        let first = prepared.reclaimed;
        let end = prepared
            .manifest
            .parts
            .min(first.saturating_add(9990 - self.rows));
        self.rows = self.rows.saturating_add(end - first).saturating_add(1);
        prepared.reclaimed = end;
        self.report.pages_removed += end - first;
        if end == prepared.manifest.parts {
            self.report.snapshots_removed += 1;
        } else {
            self.report.incomplete = true;
        }
        self.plans.push((namespace, key.to_vec(), first, prepared));
        Ok(())
    }
}

fn apply_gc_plans(
    transaction: &mut StateTransaction<'_>,
    plans: Vec<(StateNamespace, Vec<u8>, u32, Prepared)>,
) -> Result<(), RocksError> {
    for (namespace, key, first, prepared) in plans {
        for part in first..prepared.reclaimed {
            transaction.delete(
                namespace,
                &part_key(&prepared.name, &prepared.manifest.token, part),
            )?;
        }
        if prepared.reclaimed == prepared.manifest.parts {
            transaction.delete(namespace, &key)?;
        } else {
            transaction.put(namespace, &key, encode(&prepared)?)?;
        }
    }
    Ok(())
}

fn apply_gc_cursors(
    transaction: &mut StateTransaction<'_>,
    cursors: Vec<(String, Option<Vec<u8>>)>,
) -> Result<(), RocksError> {
    for (cursor, next) in cursors {
        if let Some(next) = next {
            transaction.put(StateNamespace::Metadata, cursor.as_bytes(), next)?;
        } else {
            transaction.delete(StateNamespace::Metadata, cursor.as_bytes())?;
        }
    }
    Ok(())
}

pub(crate) fn manifest(
    transaction: &StateTransaction<'_>,
    namespace: StateNamespace,
    name: &str,
    budget: &mut ReadBudget,
) -> Result<Option<Manifest>, RocksError> {
    transaction
        .get(namespace, &head_key(name), budget)?
        .map(|bytes| {
            let manifest = decode::<Manifest>(&bytes)?;
            validate_manifest(&manifest)?;
            Ok(manifest)
        })
        .transpose()
}

pub(crate) fn visit<T: DeserializeOwned>(
    transaction: &StateTransaction<'_>,
    namespace: StateNamespace,
    name: &str,
    manifest: &Manifest,
    budget: &mut ReadBudget,
    mut visitor: impl FnMut(T, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    validate_manifest(manifest)?;
    let mut rows = 0_u64;
    let mut bytes = 0_u64;
    let mut digest = blake3::Hasher::new();
    for part in 0..manifest.parts {
        let encoded = transaction
            .get(namespace, &part_key(name, &manifest.token, part), budget)?
            .ok_or(corrupt())?;
        if encoded.len() > PART_BYTES as usize {
            return Err(corrupt());
        }
        bytes = bytes.checked_add(encoded.len() as u64).ok_or(corrupt())?;
        digest.update(&encoded);
        let values: Vec<T> = decode(&encoded)?;
        if values.is_empty() || values.len() > ROWS_PER_PART {
            return Err(corrupt());
        }
        rows = rows.checked_add(values.len() as u64).ok_or(corrupt())?;
        for value in values {
            budget.checkpoint()?;
            visitor(value, budget)?;
        }
    }
    if rows != manifest.rows
        || bytes != manifest.bytes
        || ContentDigest::from_bytes(*digest.finalize().as_bytes()) != manifest.digest
    {
        return Err(corrupt());
    }
    Ok(())
}

pub(crate) fn list(
    transaction: &StateTransaction<'_>,
    namespace: StateNamespace,
    prefix: &str,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<String, Manifest>, RocksError> {
    let mut output = BTreeMap::new();
    let prefix = format!("dataset/h/{prefix}");
    let mut bytes = 0_usize;
    super::visit_state::<Manifest>(
        transaction,
        namespace,
        prefix.as_bytes(),
        budget,
        |key, manifest, _| {
            validate_manifest(&manifest)?;
            bytes = bytes.saturating_add(key.len() + encode(&manifest)?.len());
            if bytes > 16 * 1_048_576 || output.len() >= 8192 {
                return Err(RocksError::Limit {
                    resource: "dataset_headers",
                });
            }
            let name = std::str::from_utf8(key.get(b"dataset/h/".len()..).ok_or(corrupt())?)
                .map_err(|_| corrupt())?;
            output.insert(name.to_owned(), manifest);
            Ok(())
        },
    )?;
    Ok(output)
}

fn head_key(name: &str) -> Vec<u8> {
    format!("dataset/h/{name}").into_bytes()
}
fn prepared_key(name: &str, token: &str) -> Vec<u8> {
    format!("dataset/s/{name}/{token}").into_bytes()
}

fn valid_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_manifest(manifest: &Manifest) -> Result<(), RocksError> {
    if manifest.rows > 3_000_000
        || manifest.parts > 12_000
        || manifest.bytes > MAXIMUM_DATASET_BYTES
        || !valid_token(&manifest.token)
        || manifest.created.parse::<jiff::Timestamp>().is_err()
        || (manifest.parts == 0 && (manifest.rows != 0 || manifest.bytes != 0))
        || u64::from(manifest.parts) > manifest.rows
        || manifest.rows > u64::from(manifest.parts) * ROWS_PER_PART as u64
    {
        return Err(corrupt());
    }
    Ok(())
}

fn input_epoch(namespace: StateNamespace, name: &str) -> Option<&'static [u8]> {
    if namespace == StateNamespace::Coverage {
        Some(b"coverage_epoch")
    } else if namespace == StateNamespace::Derived && name == "similarity" {
        Some(b"similarity_epoch")
    } else {
        None
    }
}
fn part_key(name: &str, token: &str, part: u32) -> Vec<u8> {
    format!("dataset/b/{name}/{token}/{part:08x}").into_bytes()
}
fn corrupt() -> RocksError {
    RocksError::Corrupt { field: "dataset" }
}

fn validate_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name.len() > 256
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
    {
        return Err(StorageError::InvalidInput {
            field: "dataset_name",
        });
    }
    Ok(())
}

fn token() -> Result<String, StorageError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| StorageError::DatabaseOperation {
        operation: "dataset-identity",
    })?;
    Ok(blake3::Hash::from_bytes(bytes).to_hex().to_string())
}

struct BoundedWriter(Vec<u8>);
impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > PART_BYTES as usize {
            return Err(std::io::Error::other("dataset part exceeds its byte bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
