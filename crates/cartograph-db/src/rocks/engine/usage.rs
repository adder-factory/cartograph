//! Read-only native size and allocation telemetry, independent of graph retention.
use super::{ProjectStore, Result, RocksError, family_names, operation};
use serde::Serialize;

/// Native physical files and separately labelled `RocksDB` estimates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStorageUsage {
    /// File lengths observed while listing this project's native directory.
    pub file_bytes: u64,
    /// Allocated filesystem blocks, when the platform provides them.
    pub allocated_bytes: Option<u64>,
    /// Native WAL file lengths; informational logs are excluded.
    pub wal_bytes: u64,
    /// Number of files observed; compaction may concurrently replace a file.
    pub files: u32,
    /// Total SST bytes reported by all column families.
    pub sst_bytes: u64,
    /// Estimated live key/value data, excluding obsolete versions and overhead.
    pub estimated_live_data_bytes: u64,
    /// Active and retained immutable memtables across column families.
    pub memtable_bytes: u64,
    /// Estimated bytes awaiting automatic compaction.
    pub estimated_pending_compaction_bytes: u64,
}

impl ProjectStore {
    /// Observe bounded native storage without scanning graph records or mutating files.
    /// # Errors
    /// Rejects unreadable properties, unsafe files, cancellation or the directory-entry limit.
    pub fn storage_usage(&self) -> Result<NativeStorageUsage> {
        self.check_control()?;
        let mut report = NativeStorageUsage {
            file_bytes: 0,
            allocated_bytes: if cfg!(unix) { Some(0) } else { None },
            wal_bytes: 0,
            files: 0,
            sst_bytes: 0,
            estimated_live_data_bytes: 0,
            memtable_bytes: 0,
            estimated_pending_compaction_bytes: 0,
        };
        self.collect_storage_properties(&mut report)?;
        self.collect_storage_files(&mut report)?;
        Ok(report)
    }

    fn collect_storage_properties(&self, report: &mut NativeStorageUsage) -> Result<()> {
        use rocksdb::properties;
        for name in family_names() {
            self.check_control()?;
            let family = self
                .inner
                .database
                .cf_handle(name)
                .ok_or_else(|| operation("storage_usage"))?;
            for (property, total) in [
                (properties::TOTAL_SST_FILES_SIZE, &mut report.sst_bytes),
                (
                    properties::ESTIMATE_LIVE_DATA_SIZE,
                    &mut report.estimated_live_data_bytes,
                ),
                (properties::SIZE_ALL_MEM_TABLES, &mut report.memtable_bytes),
                (
                    properties::ESTIMATE_PENDING_COMPACTION_BYTES,
                    &mut report.estimated_pending_compaction_bytes,
                ),
            ] {
                let value = self
                    .inner
                    .database
                    .property_int_value_cf(&family, property)
                    .map_err(|_| operation("storage_usage"))?
                    .ok_or_else(|| operation("storage_usage_property"))?;
                *total = total.checked_add(value).ok_or(RocksError::Limit {
                    resource: "storage_usage_bytes",
                })?;
            }
        }
        Ok(())
    }

    fn collect_storage_files(&self, report: &mut NativeStorageUsage) -> Result<()> {
        for entry in
            std::fs::read_dir(self.inner.database.path()).map_err(|_| operation("storage_usage"))?
        {
            self.check_control()?;
            if report.files >= 100_000 {
                return Err(RocksError::Limit {
                    resource: "storage_directory_entries",
                });
            }
            let entry = entry.map_err(|_| operation("storage_usage"))?;
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(operation("storage_usage")),
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(RocksError::Directory);
            }
            report.files += 1;
            report.file_bytes =
                report
                    .file_bytes
                    .checked_add(metadata.len())
                    .ok_or(RocksError::Limit {
                        resource: "storage_usage_bytes",
                    })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                report.allocated_bytes = report.allocated_bytes.and_then(|bytes| {
                    metadata
                        .blocks()
                        .checked_mul(512)
                        .and_then(|blocks| bytes.checked_add(blocks))
                });
                if report.allocated_bytes.is_none() {
                    return Err(RocksError::Limit {
                        resource: "storage_usage_bytes",
                    });
                }
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.strip_suffix(".log").is_some_and(|stem| {
                !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                report.wal_bytes =
                    report
                        .wal_bytes
                        .checked_add(metadata.len())
                        .ok_or(RocksError::Limit {
                            resource: "storage_usage_bytes",
                        })?;
            }
        }
        Ok(())
    }
}
