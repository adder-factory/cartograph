//! Chunked immutable extraction cache on the existing native project owner.

use super::{
    CACHE_OPERATION_TIMEOUT, CACHE_USAGE_TOUCH_INTERVAL, MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES,
    MAXIMUM_CACHE_BATCH_ENTRIES, MAXIMUM_CACHE_BATCH_LOAD_BYTES, MAXIMUM_CACHE_BATCH_PAYLOAD_BYTES,
    NativeParseCacheBatchWrite, NativeParseCacheEntry, NativeParseCacheError, NativeParseCacheKey,
    NativeParseCacheRecord, NativeParseCacheWrite, validate_key,
};
use crate::{
    NativeDatabase, StorageError,
    native_database::{decode, encode, now},
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::ContentDigest;
use serde::{Deserialize, Serialize};

const CHUNK_BYTES: usize = 4 * 1_048_576;
mod retention;
#[cfg(test)]
mod tests;
const HEADER_BATCH_BYTES: u32 = 4 * 1_048_576;
const NAMESPACE: StateNamespace = StateNamespace::ParseCache;

#[derive(Clone, Serialize, Deserialize)]
struct Header {
    key: NativeParseCacheKey,
    digest: ContentDigest,
    bytes: usize,
    chunks: u32,
    last_used: String,
}

#[derive(Serialize, Deserialize)]
struct Prepared {
    header: Header,
    created: String,
}

impl NativeDatabase {
    /// Load an exact cache key, checking every chunk and the payload digest.
    /// # Errors
    /// Rejects malformed identities, corrupt payloads, deadlines, or exhausted reads.
    pub async fn load_native_parse_cache(
        &self,
        key: &NativeParseCacheKey,
    ) -> Result<Option<NativeParseCacheRecord>, NativeParseCacheError> {
        let mut rows = self
            .load_native_parse_cache_batch(std::slice::from_ref(key))
            .await?;
        rows.pop().ok_or(NativeParseCacheError::CorruptStoredValue)
    }

    /// Load one bounded batch in request order under one consistent snapshot.
    /// # Errors
    /// Rejects project/key mismatch, corrupt chunks, or the aggregate payload bound.
    pub async fn load_native_parse_cache_batch(
        &self,
        keys: &[NativeParseCacheKey],
    ) -> Result<Vec<Option<NativeParseCacheRecord>>, NativeParseCacheError> {
        validate_keys(self, keys)?;
        let keys = keys.to_vec();
        self.state(None, HEADER_BATCH_BYTES, 256, move |transaction, budget| {
            let mut output = Vec::with_capacity(keys.len());
            let mut loaded = 0_usize;
            let timestamp = now();
            for key in keys {
                let encoded_key = header_key(&key)?;
                let Some(bytes) = transaction.get(NAMESPACE, &encoded_key, budget)? else {
                    output.push(None);
                    continue;
                };
                let mut header: Header = decode(&bytes)?;
                check_header(&header, &key)?;
                loaded = loaded.checked_add(header.bytes).ok_or(corrupt())?;
                if loaded
                    > usize::try_from(MAXIMUM_CACHE_BATCH_LOAD_BYTES).map_err(|_| corrupt())?
                {
                    return Err(RocksError::Limit {
                        resource: "parse_cache_load_bytes",
                    });
                }
                let payload = load_payload(transaction, &header, budget)?;
                if touch_required(&header.last_used, &timestamp)? {
                    header.last_used.clone_from(&timestamp);
                    transaction.put(NAMESPACE, &encoded_key, encode(&header)?)?;
                }
                output.push(Some(NativeParseCacheRecord {
                    payload,
                    payload_digest: header.digest,
                    source_bytes: header.key.source_bytes,
                }));
            }
            Ok(output)
        })
        .await
        .map_err(NativeParseCacheError::from)
    }

    /// Store an immutable payload through chunk preparation and atomic visibility.
    /// # Errors
    /// Rejects invalid payloads or a different payload for an existing exact key.
    pub async fn store_native_parse_cache(
        &self,
        key: &NativeParseCacheKey,
        payload: &[u8],
    ) -> Result<NativeParseCacheWrite, NativeParseCacheError> {
        let report = self
            .store_native_parse_cache_batch(vec![NativeParseCacheEntry::new(
                key.clone(),
                payload.to_vec(),
            )])
            .await?;
        Ok(if report.inserted == 1 {
            NativeParseCacheWrite::Inserted
        } else {
            NativeParseCacheWrite::AlreadyPresent
        })
    }

    /// Publish all batch headers atomically after their immutable chunks are durable.
    /// Interrupted preparation remains invisible and is eligible for bounded cleanup.
    /// # Errors
    /// Rejects duplicate/conflicting identities, payload bounds, or native failures.
    pub async fn store_native_parse_cache_batch(
        &self,
        entries: Vec<NativeParseCacheEntry>,
    ) -> Result<NativeParseCacheBatchWrite, NativeParseCacheError> {
        validate_entries(self, &entries)?;
        Self::with_preparation_deadline(CACHE_OPERATION_TIMEOUT, self.prepare_cache(entries)).await
    }

    async fn prepare_cache(
        &self,
        entries: Vec<NativeParseCacheEntry>,
    ) -> Result<NativeParseCacheBatchWrite, NativeParseCacheError> {
        let mut headers = Vec::with_capacity(entries.len());
        for entry in entries {
            let header = make_header(entry.key, &entry.payload)?;
            let marker =
                prepared_key(&header).map_err(|_| NativeParseCacheError::CorruptStoredValue)?;
            let prepared = encode(&Prepared {
                header: header.clone(),
                created: now(),
            })
            .map_err(|_| NativeParseCacheError::CorruptStoredValue)?;
            self.state(None, HEADER_BATCH_BYTES, 1, move |transaction, _| {
                transaction.put(NAMESPACE, &marker, prepared)
            })
            .await
            .map_err(NativeParseCacheError::from)?;
            for (part, bytes) in entry.payload.chunks(CHUNK_BYTES).enumerate() {
                let key = chunk_key(
                    &header,
                    u32::try_from(part).map_err(|_| NativeParseCacheError::CorruptStoredValue)?,
                )
                .map_err(|_| NativeParseCacheError::CorruptStoredValue)?;
                let bytes = bytes.to_vec();
                self.state(None, 4 * 1_048_576 + 4096, 1, move |transaction, _| {
                    transaction.put(NAMESPACE, &key, bytes)
                })
                .await
                .map_err(NativeParseCacheError::from)?;
            }
            headers.push(header);
        }
        self.state(None, HEADER_BATCH_BYTES, 512, move |transaction, budget| {
            // Complete validation precedes all mutations, so any conflict rolls
            // back every header in the caller's batch.
            let report = check_prepared_headers(transaction, budget, &headers)?;
            for header in headers {
                transaction.put(NAMESPACE, &header_key(&header.key)?, encode(&header)?)?;
                transaction.delete(NAMESPACE, &prepared_key(&header)?)?;
            }
            Ok(report)
        })
        .await
        .map_err(NativeParseCacheError::from)
    }

    /// Remove one corrupt or obsolete exact cache identity and its chunk payload.
    /// # Errors
    /// Rejects project/key mismatch or malformed stored metadata.
    pub async fn evict_native_parse_cache(
        &self,
        key: &NativeParseCacheKey,
    ) -> Result<bool, NativeParseCacheError> {
        validate_keys(self, std::slice::from_ref(key))?;
        let requested = key.clone();
        self.state(None, HEADER_BATCH_BYTES, 18, move |transaction, budget| {
            let key = header_key(&requested)?;
            let Some(bytes) = transaction.get(NAMESPACE, &key, budget)? else {
                return Ok(false);
            };
            let header: Header = decode(&bytes)?;
            check_header(&header, &requested)?;
            for part in 0..header.chunks {
                transaction.delete(NAMESPACE, &chunk_key(&header, part)?)?;
            }
            transaction.delete(NAMESPACE, &key)?;
            transaction.delete(NAMESPACE, &prepared_key(&header)?)?;
            Ok(true)
        })
        .await
        .map_err(NativeParseCacheError::from)
    }
}

fn check_prepared_headers(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    headers: &[Header],
) -> Result<NativeParseCacheBatchWrite, RocksError> {
    let mut report = NativeParseCacheBatchWrite::default();
    for header in headers {
        if let Some(bytes) = transaction.get(NAMESPACE, &header_key(&header.key)?, budget)? {
            let existing: Header = decode(&bytes)?;
            check_header(&existing, &header.key)?;
            if existing.digest != header.digest || existing.bytes != header.bytes {
                return Err(RocksError::ReplayConflict);
            }
            report.already_present += 1;
        } else {
            report.inserted += 1;
        }
        // Preparation and eviction can interleave; every chunk must survive
        // before any header in this transaction becomes visible.
        let _ = load_payload(transaction, header, budget)?;
    }
    Ok(report)
}

fn validate_keys(
    database: &NativeDatabase,
    keys: &[NativeParseCacheKey],
) -> Result<(), NativeParseCacheError> {
    if keys.len() > MAXIMUM_CACHE_BATCH_ENTRIES {
        return Err(NativeParseCacheError::InvalidInput {
            field: "batch_count",
        });
    }
    for key in keys {
        database
            .require_project(&key.project_id)
            .map_err(NativeParseCacheError::from)?;
        validate_key(key)?;
    }
    Ok(())
}

fn validate_entries(
    database: &NativeDatabase,
    entries: &[NativeParseCacheEntry],
) -> Result<(), NativeParseCacheError> {
    validate_keys(
        database,
        &entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut bytes = 0_usize;
    let mut keys = std::collections::BTreeSet::new();
    for entry in entries {
        if entry.payload.is_empty() || entry.payload.len() > MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES {
            return Err(NativeParseCacheError::InvalidInput { field: "payload" });
        }
        bytes =
            bytes
                .checked_add(entry.payload.len())
                .ok_or(NativeParseCacheError::InvalidInput {
                    field: "batch_bytes",
                })?;
        if bytes > MAXIMUM_CACHE_BATCH_PAYLOAD_BYTES
            || !keys.insert(
                header_key(&entry.key).map_err(|_| NativeParseCacheError::CorruptStoredValue)?,
            )
        {
            return Err(NativeParseCacheError::InvalidInput {
                field: "batch_bytes_or_duplicate_key",
            });
        }
    }
    Ok(())
}

fn make_header(key: NativeParseCacheKey, payload: &[u8]) -> Result<Header, NativeParseCacheError> {
    Ok(Header {
        key,
        digest: ContentDigest::from_bytes(*blake3::hash(payload).as_bytes()),
        bytes: payload.len(),
        chunks: u32::try_from(payload.len().div_ceil(CHUNK_BYTES))
            .map_err(|_| NativeParseCacheError::InvalidInput { field: "payload" })?,
        last_used: now(),
    })
}

fn header_key(key: &NativeParseCacheKey) -> Result<Vec<u8>, RocksError> {
    let digest = blake3::hash(&encode(key)?);
    Ok(format!("h/{}", digest.to_hex()).into_bytes())
}

fn payload_prefix(header: &Header) -> Result<String, RocksError> {
    Ok(format!(
        "{}/{}",
        blake3::hash(&encode(&header.key)?).to_hex(),
        header.digest.as_str()
    ))
}

fn chunk_key(header: &Header, part: u32) -> Result<Vec<u8>, RocksError> {
    Ok(format!("b/{}/{part:08x}", payload_prefix(header)?).into_bytes())
}

fn prepared_key(header: &Header) -> Result<Vec<u8>, RocksError> {
    Ok(format!("s/{}", payload_prefix(header)?).into_bytes())
}

fn check_header(header: &Header, key: &NativeParseCacheKey) -> Result<(), RocksError> {
    if &header.key != key
        || header.bytes == 0
        || header.bytes > MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES
        || usize::try_from(header.chunks).ok() != Some(header.bytes.div_ceil(CHUNK_BYTES))
        || header.last_used.parse::<jiff::Timestamp>().is_err()
    {
        return Err(corrupt());
    }
    Ok(())
}

fn load_payload(
    transaction: &StateTransaction<'_>,
    header: &Header,
    budget: &mut ReadBudget,
) -> Result<Vec<u8>, RocksError> {
    let mut payload = Vec::with_capacity(header.bytes);
    for part in 0..header.chunks {
        let chunk = transaction
            .get(NAMESPACE, &chunk_key(header, part)?, budget)?
            .ok_or(corrupt())?;
        let expected = (header.bytes - payload.len()).min(CHUNK_BYTES);
        if chunk.len() != expected {
            return Err(corrupt());
        }
        payload.extend_from_slice(&chunk);
    }
    if ContentDigest::from_bytes(*blake3::hash(&payload).as_bytes()) != header.digest {
        return Err(corrupt());
    }
    Ok(payload)
}

fn touch_required(last: &str, current: &str) -> Result<bool, RocksError> {
    let last = last.parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
    let current = current.parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
    Ok(current.as_second().saturating_sub(last.as_second())
        >= i64::try_from(CACHE_USAGE_TOUCH_INTERVAL.as_secs()).map_err(|_| corrupt())?)
}

fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "parse_cache",
    }
}

impl From<StorageError> for NativeParseCacheError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::InvalidInput { field } => NativeParseCacheError::InvalidInput { field },
            StorageError::ImmutableConflict => NativeParseCacheError::PayloadConflict,
            StorageError::CorruptStoredValue { .. } => NativeParseCacheError::CorruptStoredValue,
            _ => NativeParseCacheError::DatabaseOperation {
                operation: "native-cache",
            },
        }
    }
}
