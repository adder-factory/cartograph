//! Atomic recording of a verified `RocksDB` rebuild, never runtime backend selection.

use std::path::Path;

use serde_json::Value;

use crate::{ProjectConfigError, read_project_config, update_project_config};

/// Observed migration marker, used only to fence a verified config update.
/// A legacy PostgreSQL marker is accepted as migration input; it never selects
/// a PostgreSQL runtime.
#[derive(Debug)]
pub struct StorageMigrationGuard {
    observed: Option<String>,
}

fn marker(value: Option<&Value>) -> Result<Option<String>, ProjectConfigError> {
    match value {
        None => Ok(None),
        Some(Value::String(name)) if matches!(name.as_str(), "postgresql" | "rocksdb") => {
            Ok(Some(name.clone()))
        }
        Some(_) => Err(ProjectConfigError::InvalidConfig),
    }
}

/// Capture the existing migration marker without loading database credentials.
///
/// # Errors
/// Rejects unsafe configuration and unknown storage marker values.
pub fn storage_migration_guard(root: &Path) -> Result<StorageMigrationGuard, ProjectConfigError> {
    let value = read_project_config(root)?;
    let object = value
        .as_ref()
        .map(|value| value.as_object().ok_or(ProjectConfigError::InvalidConfig))
        .transpose()?;
    Ok(StorageMigrationGuard {
        observed: marker(object.and_then(|object| object.get("storageBackend")))?,
    })
}

/// Record a verified `RocksDB` rebuild while preserving unrelated settings.
/// The caller must first publish and query a fresh destination generation.
/// The marker records migration completion; `RocksDB` is always the runtime.
///
/// # Errors
/// Refuses concurrent marker changes and unsafe configuration writes.
pub fn record_rocksdb_migration(
    root: &Path,
    guard: StorageMigrationGuard,
) -> Result<(), ProjectConfigError> {
    let StorageMigrationGuard { observed } = guard;
    update_project_config(root, |value| {
        let mut value = value.unwrap_or_else(|| serde_json::json!({}));
        let object = value
            .as_object_mut()
            .ok_or(ProjectConfigError::InvalidConfig)?;
        if marker(object.get("storageBackend"))? != observed {
            return Err(ProjectConfigError::ConcurrentModification);
        }
        object.insert(
            "storageBackend".to_owned(),
            Value::String("rocksdb".to_owned()),
        );
        Ok((value, ()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_settings_and_refuses_a_stale_marker()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let initial = storage_migration_guard(root.path())?;
        update_project_config::<_, ProjectConfigError>(root.path(), |_| {
            Ok((
                serde_json::json!({"maxFileSize": 1234, "custom": {"preserve": true}}),
                (),
            ))
        })?;
        record_rocksdb_migration(root.path(), initial)?;
        assert_eq!(
            read_project_config(root.path())?.and_then(|value| value.get("custom").cloned()),
            Some(serde_json::json!({"preserve": true}))
        );
        let stale = storage_migration_guard(root.path())?;
        update_project_config::<_, ProjectConfigError>(root.path(), |_| {
            Ok((serde_json::json!({"storageBackend": "postgresql"}), ()))
        })?;
        assert_eq!(
            record_rocksdb_migration(root.path(), stale),
            Err(ProjectConfigError::ConcurrentModification)
        );
        record_rocksdb_migration(root.path(), storage_migration_guard(root.path())?)?;
        assert_eq!(
            read_project_config(root.path())?
                .and_then(|value| value.get("storageBackend").cloned()),
            Some(Value::String("rocksdb".to_owned()))
        );
        update_project_config::<_, ProjectConfigError>(root.path(), |_| {
            Ok((serde_json::json!({"storageBackend": "invalid"}), ()))
        })?;
        assert!(matches!(
            storage_migration_guard(root.path()),
            Err(ProjectConfigError::InvalidConfig)
        ));
        Ok(())
    }
}
