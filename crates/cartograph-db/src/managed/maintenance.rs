use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use cartograph_config::DatabaseSettings;
use secrecy::ExposeSecret;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use tempfile::NamedTempFile;
use tokio::time::timeout;

use super::{
    ManagedBackupReport, ManagedContainerState, ManagedDatabaseArchives, ManagedDatabaseError,
    ManagedDatabaseMaintenance, ManagedDerivedIndexAvailability, ManagedDerivedIndexHealth,
    ManagedDestructiveConfirmation, ManagedDestructiveOperation, ManagedInitialization,
    ManagedRemoveReport, ManagedRestoreReport, ManagedUpgradeReport, container_state,
    credentials::DatabaseCredentials,
    docker::{
        ContainerArchivePath, ContainerCreateSpec, ContainerInspection, DatabaseArchiveOperation,
        DatabaseArchiveRequest, initialize_extensions,
    },
    has_expected_resource_limits, initialize_managed_database, validate_configured_port,
    validate_destructive_confirmation, validate_owned_container,
};
use crate::{CartographDatabase, connect};

const ARCHIVE_MAGIC: &[u8; 5] = b"PGDMP";
const ARCHIVE_MAGIC_BYTES: u64 = 5;
const BACKUP_CONTAINER_PATH: &str = "/tmp/cartograph-managed-backup.dump";
const RESTORE_CONTAINER_PATH: &str = "/tmp/cartograph-managed-restore.dump";
const ROLLBACK_CONTAINER_PATH: &str = "/tmp/cartograph-managed-rollback.dump";
const UPGRADE_ROLLBACK_SUFFIX: &str = "-upgrade-rollback";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OriginalContainerState {
    Running,
    Paused,
    Stopped,
}

enum UpgradeInitializationFailure {
    RollbackSafe(ManagedDatabaseError),
    RetainReplacement(ManagedDatabaseError),
}

struct PreparedUpgrade {
    inspection: ContainerInspection,
    retained_rollback: Option<ContainerInspection>,
    rollback_name: String,
    credentials: DatabaseCredentials,
}

impl ManagedDatabaseArchives<'_> {
    /// Create and atomically persist a verified custom-format PostgreSQL archive.
    /// # Errors
    ///
    /// Returns an error if the destination is unsafe/existing, the owned
    /// database is unhealthy, or archive creation, verification, cleanup, or rename fails.
    pub async fn backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<ManagedBackupReport, ManagedDatabaseError> {
        let destination = destination.as_ref();
        validate_new_backup_destination(destination)?;
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        self.database
            .maintenance()
            .require_healthy_owned_container(false)
            .await?;
        let _credentials = self.database.credentials.load()?;

        let result = self.backup_locked(destination).await;
        let cleanup = self
            .database
            .docker
            .archives()
            .remove_container_archive(
                &self.database.identity.container_name,
                BACKUP_CONTAINER_PATH,
            )
            .await;
        prefer_archive_cleanup(result, cleanup)
    }

    async fn backup_locked(
        &self,
        destination: &Path,
    ) -> Result<ManagedBackupReport, ManagedDatabaseError> {
        self.database
            .docker
            .archives()
            .remove_container_archive(
                &self.database.identity.container_name,
                BACKUP_CONTAINER_PATH,
            )
            .await?;
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                BACKUP_CONTAINER_PATH,
                DatabaseArchiveOperation::Create,
            ))
            .await?;
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                BACKUP_CONTAINER_PATH,
                DatabaseArchiveOperation::Verify,
            ))
            .await?;

        let parent = destination
            .parent()
            .filter(|parent| parent.is_dir())
            .ok_or(ManagedDatabaseError::BackupDestination)?;
        let temporary =
            NamedTempFile::new_in(parent).map_err(|_| ManagedDatabaseError::BackupDestination)?;
        self.database
            .docker
            .archives()
            .copy_from_container(
                ContainerArchivePath {
                    name: &self.database.identity.container_name,
                    path: BACKUP_CONTAINER_PATH,
                },
                temporary.path(),
            )
            .await?;
        set_private_archive_permissions(temporary.path())?;
        let bytes = verify_archive_file(temporary.path())?;
        open_archive(temporary.path())?
            .sync_all()
            .map_err(|_| ManagedDatabaseError::BackupDestination)?;
        temporary
            .persist_noclobber(destination)
            .map_err(|_| ManagedDatabaseError::BackupDestination)?;
        sync_parent(parent)?;
        Ok(ManagedBackupReport { bytes })
    }

    /// Replace live contents from a verified archive and recover the old state
    /// if either restore or post-restore capability proof fails.
    /// # Errors
    ///
    /// Returns an error if confirmation/archive/ownership checks fail, restore
    /// or capability proof fails, or the previous state cannot be recovered.
    pub async fn restore(
        &self,
        source: impl AsRef<Path>,
        confirmation: ManagedDestructiveConfirmation,
    ) -> Result<ManagedRestoreReport, ManagedDatabaseError> {
        validate_destructive_confirmation(
            self.database,
            &confirmation,
            ManagedDestructiveOperation::Restore,
        )?;
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        let staged = stage_restore_archive(source.as_ref(), self.database.credentials.path())?;
        self.database
            .maintenance()
            .require_healthy_owned_container(true)
            .await?;
        let credentials = self.database.credentials.load()?;

        let result = self.restore_locked(staged.path(), &credentials).await;
        let cleanup = self.cleanup_restore_archives().await;
        prefer_archive_cleanup(result, cleanup)
    }

    async fn restore_locked(
        &self,
        source: &Path,
        credentials: &DatabaseCredentials,
    ) -> Result<ManagedRestoreReport, ManagedDatabaseError> {
        self.database
            .docker
            .archives()
            .copy_to_container(
                source,
                ContainerArchivePath {
                    name: &self.database.identity.container_name,
                    path: RESTORE_CONTAINER_PATH,
                },
            )
            .await?;
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                RESTORE_CONTAINER_PATH,
                DatabaseArchiveOperation::Verify,
            ))
            .await?;
        self.database
            .docker
            .archives()
            .remove_container_archive(
                &self.database.identity.container_name,
                ROLLBACK_CONTAINER_PATH,
            )
            .await?;
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                ROLLBACK_CONTAINER_PATH,
                DatabaseArchiveOperation::Create,
            ))
            .await?;
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                ROLLBACK_CONTAINER_PATH,
                DatabaseArchiveOperation::Verify,
            ))
            .await?;

        if self
            .database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                RESTORE_CONTAINER_PATH,
                DatabaseArchiveOperation::Restore,
            ))
            .await
            .is_err()
        {
            self.restore_rollback(credentials).await?;
            return Err(ManagedDatabaseError::RestoreFailed);
        }
        if let Ok(initialized) = self.verify_restored_database(credentials).await {
            Ok(restore_report(initialized, &self.database.schema))
        } else {
            self.restore_rollback(credentials).await?;
            Err(ManagedDatabaseError::RestoreVerificationFailed)
        }
    }

    async fn restore_rollback(
        &self,
        credentials: &DatabaseCredentials,
    ) -> Result<(), ManagedDatabaseError> {
        self.database
            .docker
            .archives()
            .database_archive(DatabaseArchiveRequest::new(
                &self.database.identity.container_name,
                ROLLBACK_CONTAINER_PATH,
                DatabaseArchiveOperation::Restore,
            ))
            .await
            .map_err(|_| ManagedDatabaseError::RestoreRollbackFailed)?;
        self.verify_restored_database(credentials)
            .await
            .map(|_| ())
            .map_err(|_| ManagedDatabaseError::RestoreRollbackFailed)
    }

    async fn verify_restored_database(
        &self,
        credentials: &DatabaseCredentials,
    ) -> Result<ManagedInitialization, ManagedDatabaseError> {
        let initialized =
            initialize_managed_database(credentials, self.database.port, &self.database.schema)
                .await?;
        let database =
            open_managed_database(credentials, self.database.port, &self.database.schema).await?;
        let health = timeout(
            self.database.timeouts.maintenance,
            read_bm25_health(&database),
        )
        .await;
        database.pool.close().await;
        let health = health.map_err(|_| ManagedDatabaseError::MaintenanceTimeout)??;
        if !health.healthy() {
            return Err(ManagedDatabaseError::DerivedIndexUnhealthy);
        }
        Ok(initialized)
    }

    async fn cleanup_restore_archives(&self) -> Result<(), ManagedDatabaseError> {
        let restore = self
            .database
            .docker
            .archives()
            .remove_container_archive(
                &self.database.identity.container_name,
                RESTORE_CONTAINER_PATH,
            )
            .await;
        let rollback = self
            .database
            .docker
            .archives()
            .remove_container_archive(
                &self.database.identity.container_name,
                ROLLBACK_CONTAINER_PATH,
            )
            .await;
        restore.and(rollback)
    }
}

impl ManagedDatabaseMaintenance<'_> {
    /// Remove only resources whose labels, project identity, and data mount all match.
    /// # Errors
    ///
    /// Returns an error if confirmation or ownership checks fail, or the exact
    /// project container, volume, credential, or cleanup operation cannot complete.
    pub async fn remove(
        &self,
        confirmation: ManagedDestructiveConfirmation,
    ) -> Result<ManagedRemoveReport, ManagedDatabaseError> {
        validate_destructive_confirmation(
            self.database,
            &confirmation,
            ManagedDestructiveOperation::Remove,
        )?;
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        let inspection = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await?;
        if let Some(inspection) = inspection.as_ref() {
            validate_owned_container(&self.database.identity, inspection, false)?;
        }
        let volume_exists = self
            .database
            .docker
            .volumes()
            .volume_exists_owned(&self.database.identity)
            .await?;
        if inspection.is_some() && !volume_exists {
            return Err(ManagedDatabaseError::ManagedVolumeMissing);
        }
        let credentials_exist = self.database.credentials.validate_for_removal()?;

        if inspection.is_some() {
            self.database
                .docker
                .containers()
                .remove_container(&self.database.identity.container_name)
                .await?;
        }
        if volume_exists {
            self.database
                .docker
                .volumes()
                .remove_volume(&self.database.identity.volume_name)
                .await?;
        }
        let credentials_removed = if credentials_exist {
            self.database.credentials.remove()?
        } else {
            false
        };
        Ok(ManagedRemoveReport {
            container_removed: inspection.is_some(),
            volume_removed: volume_exists,
            credentials_removed,
        })
    }

    /// Replace an owned non-pinned container while retaining the old container
    /// until the new exact-digest instance passes readiness and migration proof.
    /// # Errors
    ///
    /// Returns an error if confirmation/ownership checks fail or replacement,
    /// readiness, migration, capability proof, cutover, or rollback fails.
    pub async fn upgrade(
        &self,
        confirmation: ManagedDestructiveConfirmation,
    ) -> Result<ManagedUpgradeReport, ManagedDatabaseError> {
        validate_destructive_confirmation(
            self.database,
            &confirmation,
            ManagedDestructiveOperation::Upgrade,
        )?;
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        let prepared = self.prepare_upgrade().await?;
        if let Some(report) = self.finish_supported_upgrade(&prepared).await? {
            return Ok(report);
        }

        let original_state = original_container_state(&prepared.inspection)?;
        if prepared.retained_rollback.is_some() {
            return Err(ManagedDatabaseError::UpgradeRollbackFailed);
        }
        self.database
            .docker
            .ensure_image(super::MANAGED_DATABASE_IMAGE)
            .await?;
        self.verify_upgrade_storage_headroom(&prepared.inspection, &prepared.credentials)
            .await?;
        self.quiesce_for_upgrade(original_state).await?;
        if let Err(error) = self
            .database
            .docker
            .containers()
            .rename_container(
                &self.database.identity.container_name,
                &prepared.rollback_name,
            )
            .await
        {
            self.restore_original_container_state(
                &self.database.identity.container_name,
                original_state,
            )
            .await?;
            return Err(error);
        }

        let replacement = self
            .create_and_initialize_upgrade(&prepared.credentials)
            .await;
        match replacement {
            Ok(initialized) => {
                self.database
                    .docker
                    .containers()
                    .remove_container(&prepared.rollback_name)
                    .await?;
                Ok(upgrade_report(true, initialized, &self.database.schema))
            }
            Err(UpgradeInitializationFailure::RollbackSafe(error)) => {
                self.rollback_upgrade(&prepared.rollback_name, original_state)
                    .await?;
                Err(error)
            }
            Err(UpgradeInitializationFailure::RetainReplacement(error)) => {
                self.verify_retained_upgrade_state(&prepared.rollback_name)
                    .await?;
                Err(error)
            }
        }
    }

    async fn prepare_upgrade(&self) -> Result<PreparedUpgrade, ManagedDatabaseError> {
        let mut inspection = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await?;
        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            self.database.identity.container_name
        );
        let mut retained_rollback = self
            .database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await?;
        let upgrade_preflight = inspection
            .as_ref()
            .or(retained_rollback.as_ref())
            .ok_or(ManagedDatabaseError::ManagedContainerMissing)?;
        if inspection.is_some() {
            validate_owned_container(&self.database.identity, upgrade_preflight, false)?;
        } else {
            validate_retained_upgrade_rollback(&self.database.identity, upgrade_preflight)?;
        }
        if !self
            .database
            .docker
            .volumes()
            .volume_exists_owned(&self.database.identity)
            .await?
        {
            return Err(ManagedDatabaseError::ManagedVolumeMissing);
        }
        validate_configured_port(self.database.port, upgrade_preflight)?;
        let credentials = self.database.credentials.load()?;
        if inspection.is_none() {
            inspection = Some(
                self.recover_interrupted_upgrade_rename(&rollback_name)
                    .await?,
            );
            retained_rollback = None;
        }
        let inspection = inspection.ok_or(ManagedDatabaseError::UpgradeRecoveryFailed)?;
        Ok(PreparedUpgrade {
            inspection,
            retained_rollback,
            rollback_name,
            credentials,
        })
    }

    async fn verify_upgrade_storage_headroom(
        &self,
        inspection: &ContainerInspection,
        credentials: &DatabaseCredentials,
    ) -> Result<(), ManagedDatabaseError> {
        if container_state(inspection) != ManagedContainerState::Healthy {
            return Ok(());
        }
        let available_bytes = self
            .database
            .docker
            .containers()
            .available_data_bytes(&self.database.identity.container_name)
            .await?;
        let database =
            open_managed_database(credentials, self.database.port, &self.database.schema).await?;
        let totals = database
            .storage_totals(super::MANAGED_UPGRADE_STORAGE_INSPECTION_TIMEOUT)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeStorageInspection);
        database.close().await;
        let totals = totals?;
        let required_bytes =
            required_upgrade_storage_headroom(totals.database_bytes, totals.index_bytes);
        validate_upgrade_storage_headroom(available_bytes, required_bytes)
    }

    async fn recover_interrupted_upgrade_rename(
        &self,
        rollback_name: &str,
    ) -> Result<ContainerInspection, ManagedDatabaseError> {
        self.database
            .docker
            .containers()
            .rename_container(rollback_name, &self.database.identity.container_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?;
        let recovered = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?
            .ok_or(ManagedDatabaseError::UpgradeRecoveryFailed)?;
        validate_retained_upgrade_rollback(&self.database.identity, &recovered)?;
        validate_configured_port(self.database.port, &recovered)
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?;
        Ok(recovered)
    }

    async fn finish_supported_upgrade(
        &self,
        prepared: &PreparedUpgrade,
    ) -> Result<Option<ManagedUpgradeReport>, ManagedDatabaseError> {
        if prepared.inspection.image != super::MANAGED_DATABASE_IMAGE
            || prepared.inspection.shared_memory_bytes < super::MANAGED_DATABASE_SHARED_MEMORY_BYTES
            || !has_expected_resource_limits(&prepared.inspection)
        {
            return Ok(None);
        }
        if let Some(rollback) = prepared.retained_rollback.as_ref() {
            validate_retained_upgrade_rollback(&self.database.identity, rollback)?;
        }
        let initialized = self
            .initialize_supported_existing(&prepared.inspection, &prepared.credentials)
            .await?;
        if prepared.retained_rollback.is_some() {
            self.database
                .docker
                .containers()
                .remove_container(&prepared.rollback_name)
                .await?;
        }
        Ok(Some(upgrade_report(
            prepared.retained_rollback.is_some(),
            initialized,
            &self.database.schema,
        )))
    }

    async fn initialize_supported_existing(
        &self,
        inspection: &ContainerInspection,
        credentials: &DatabaseCredentials,
    ) -> Result<ManagedInitialization, ManagedDatabaseError> {
        let state = container_state(inspection);
        let transition = self
            .database
            .lifecycle()
            .transition_existing_container(state)
            .await?;
        match self
            .database
            .lifecycle()
            .finish_start(credentials, transition == super::StartTransition::Pause)
            .await
        {
            Ok(initialized) => Ok(initialized),
            Err(error) => {
                self.database
                    .lifecycle()
                    .rollback_or_fail(transition)
                    .await?;
                Err(error)
            }
        }
    }

    async fn quiesce_for_upgrade(
        &self,
        state: OriginalContainerState,
    ) -> Result<(), ManagedDatabaseError> {
        let result = match state {
            OriginalContainerState::Running => {
                self.database
                    .docker
                    .containers()
                    .stop_container(&self.database.identity.container_name)
                    .await
            }
            OriginalContainerState::Paused => match self
                .database
                .docker
                .containers()
                .unpause_container(&self.database.identity.container_name)
                .await
            {
                Ok(()) => {
                    self.database
                        .docker
                        .containers()
                        .stop_container(&self.database.identity.container_name)
                        .await
                }
                Err(error) => Err(error),
            },
            OriginalContainerState::Stopped => Ok(()),
        };
        if let Err(error) = result {
            self.restore_original_container_state(&self.database.identity.container_name, state)
                .await?;
            return Err(error);
        }
        Ok(())
    }

    async fn create_and_initialize_upgrade(
        &self,
        credentials: &DatabaseCredentials,
    ) -> Result<ManagedInitialization, UpgradeInitializationFailure> {
        self.database
            .docker
            .containers()
            .create_container(&ContainerCreateSpec {
                identity: &self.database.identity,
                port: self.database.port,
                image: super::MANAGED_DATABASE_IMAGE,
            })
            .await
            .map_err(UpgradeInitializationFailure::RollbackSafe)?;
        self.database
            .docker
            .containers()
            .install_password_file(
                &self.database.identity.container_name,
                self.database.credentials.path(),
            )
            .await
            .map_err(UpgradeInitializationFailure::RollbackSafe)?;
        self.database
            .docker
            .containers()
            .start_container(&self.database.identity.container_name, self.database.port)
            .await
            .map_err(UpgradeInitializationFailure::RollbackSafe)?;

        let mut catalog_mutation_attempted = false;
        let initialization = timeout(self.database.timeouts.startup, async {
            self.database.lifecycle().wait_until_healthy(false).await?;
            self.database
                .docker
                .containers()
                .verify_loopback_port(&self.database.identity.container_name, self.database.port)
                .await?;
            catalog_mutation_attempted = true;
            initialize_extensions(
                &self.database.docker,
                &self.database.identity.container_name,
            )
            .await?;
            initialize_managed_database(credentials, self.database.port, &self.database.schema)
                .await
        })
        .await;
        let result = initialization
            .map_err(|_| ManagedDatabaseError::DatabaseStartupTimeout)
            .and_then(std::convert::identity);
        result.map_err(|error| {
            if catalog_mutation_attempted {
                UpgradeInitializationFailure::RetainReplacement(error)
            } else {
                UpgradeInitializationFailure::RollbackSafe(error)
            }
        })
    }

    async fn verify_retained_upgrade_state(
        &self,
        rollback_name: &str,
    ) -> Result<(), ManagedDatabaseError> {
        let replacement = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?
            .ok_or(ManagedDatabaseError::UpgradeRecoveryFailed)?;
        validate_owned_container(&self.database.identity, &replacement, true)
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?;
        validate_configured_port(self.database.port, &replacement)
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?;
        if !has_expected_resource_limits(&replacement) {
            return Err(ManagedDatabaseError::UpgradeRecoveryFailed);
        }
        let rollback = self
            .database
            .docker
            .containers()
            .inspect_container(rollback_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?
            .ok_or(ManagedDatabaseError::UpgradeRecoveryFailed)?;
        validate_retained_upgrade_rollback(&self.database.identity, &rollback)
    }

    async fn rollback_upgrade(
        &self,
        rollback_name: &str,
        state: OriginalContainerState,
    ) -> Result<(), ManagedDatabaseError> {
        if let Some(replacement) = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?
        {
            validate_owned_container(&self.database.identity, &replacement, true)
                .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
            self.database
                .docker
                .containers()
                .remove_container(&self.database.identity.container_name)
                .await
                .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
        }
        let rollback = self
            .database
            .docker
            .containers()
            .inspect_container(rollback_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?
            .ok_or(ManagedDatabaseError::UpgradeRollbackFailed)?;
        validate_owned_container(&self.database.identity, &rollback, false)
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
        self.database
            .docker
            .containers()
            .rename_container(rollback_name, &self.database.identity.container_name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
        self.restore_original_container_state(&self.database.identity.container_name, state)
            .await
    }

    async fn restore_original_container_state(
        &self,
        name: &str,
        state: OriginalContainerState,
    ) -> Result<(), ManagedDatabaseError> {
        let inspection = self
            .database
            .docker
            .containers()
            .inspect_container(name)
            .await
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?
            .ok_or(ManagedDatabaseError::UpgradeRollbackFailed)?;
        validate_owned_container(&self.database.identity, &inspection, false)
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
        let current = original_container_state(&inspection)
            .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
        match (state, current) {
            (OriginalContainerState::Running, OriginalContainerState::Running)
            | (OriginalContainerState::Paused, OriginalContainerState::Paused)
            | (OriginalContainerState::Stopped, OriginalContainerState::Stopped) => Ok(()),
            (OriginalContainerState::Running, OriginalContainerState::Paused) => self
                .database
                .docker
                .containers()
                .unpause_container(name)
                .await
                .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed),
            (OriginalContainerState::Paused, OriginalContainerState::Running) => self
                .database
                .docker
                .containers()
                .pause_container(name)
                .await
                .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed),
            (
                OriginalContainerState::Running | OriginalContainerState::Paused,
                OriginalContainerState::Stopped,
            ) => {
                self.database
                    .docker
                    .containers()
                    .install_password_file(name, self.database.credentials.path())
                    .await
                    .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
                self.database
                    .docker
                    .containers()
                    .start_container(name, self.database.port)
                    .await
                    .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
                if matches!(state, OriginalContainerState::Paused) {
                    self.database
                        .docker
                        .containers()
                        .pause_container(name)
                        .await
                        .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
                }
                Ok(())
            }
            (OriginalContainerState::Stopped, OriginalContainerState::Running) => self
                .database
                .docker
                .containers()
                .stop_container(name)
                .await
                .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed),
            (OriginalContainerState::Stopped, OriginalContainerState::Paused) => {
                self.database
                    .docker
                    .containers()
                    .unpause_container(name)
                    .await
                    .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)?;
                self.database
                    .docker
                    .containers()
                    .stop_container(name)
                    .await
                    .map_err(|_| ManagedDatabaseError::UpgradeRollbackFailed)
            }
        }
    }

    /// Read aggregate generation-local `ParadeDB` BM25 catalog health without mutation.
    /// # Errors
    ///
    /// Returns an error if owned-resource or credential checks fail, the
    /// database is unhealthy, or the bounded BM25 catalog query times out/fails.
    pub async fn derived_index_health(
        &self,
    ) -> Result<ManagedDerivedIndexHealth, ManagedDatabaseError> {
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        self.require_healthy_owned_container(true).await?;
        let credentials = self.database.credentials.load()?;
        let database =
            open_managed_database(&credentials, self.database.port, &self.database.schema).await?;
        let result = timeout(
            self.database.timeouts.maintenance,
            read_bm25_health(&database),
        )
        .await;
        database.pool.close().await;
        result.map_err(|_| ManagedDatabaseError::MaintenanceTimeout)?
    }

    /// Repair missing/invalid generation-local BM25 relations and prove aggregate health.
    /// # Errors
    ///
    /// Returns an error if confirmation/ownership/readiness checks fail or a
    /// bounded BM25 relation repair or its post-rebuild health proof fails.
    pub async fn rebuild_derived_indexes(
        &self,
        confirmation: ManagedDestructiveConfirmation,
    ) -> Result<ManagedDerivedIndexHealth, ManagedDatabaseError> {
        validate_destructive_confirmation(
            self.database,
            &confirmation,
            ManagedDestructiveOperation::RebuildDerivedIndexes,
        )?;
        self.database.docker.ensure_available().await?;
        let _lifecycle_lock = self.database.credentials.acquire_lifecycle_lock()?;
        self.require_healthy_owned_container(true).await?;
        let credentials = self.database.credentials.load()?;
        let database =
            open_managed_database(&credentials, self.database.port, &self.database.schema).await?;
        let result = timeout(
            self.database.timeouts.maintenance,
            repair_bm25_relations(&database, self.database.timeouts.maintenance),
        )
        .await;
        database.pool.close().await;
        result.map_err(|_| ManagedDatabaseError::MaintenanceTimeout)?
    }

    async fn require_healthy_owned_container(
        &self,
        require_supported_image: bool,
    ) -> Result<ContainerInspection, ManagedDatabaseError> {
        let inspection = self
            .database
            .docker
            .containers()
            .inspect_container(&self.database.identity.container_name)
            .await?
            .ok_or(ManagedDatabaseError::ManagedContainerMissing)?;
        validate_owned_container(
            &self.database.identity,
            &inspection,
            require_supported_image,
        )?;
        super::verify_volume(&self.database.docker.volumes(), &self.database.identity).await?;
        validate_configured_port(self.database.port, &inspection)?;
        if container_state(&inspection) != ManagedContainerState::Healthy {
            return Err(ManagedDatabaseError::DatabaseNotHealthyForMaintenance);
        }
        Ok(inspection)
    }
}

fn original_container_state(
    inspection: &ContainerInspection,
) -> Result<OriginalContainerState, ManagedDatabaseError> {
    match inspection.process_state.as_str() {
        "running" | "restarting" => Ok(OriginalContainerState::Running),
        "paused" => Ok(OriginalContainerState::Paused),
        "created" | "exited" | "dead" => Ok(OriginalContainerState::Stopped),
        _ => Err(ManagedDatabaseError::UnsupportedContainerState),
    }
}

fn required_upgrade_storage_headroom(database_bytes: u64, index_bytes: u64) -> u64 {
    let database_reserve = database_bytes / super::MANAGED_UPGRADE_DATABASE_RESERVE_DIVISOR;
    super::MINIMUM_MANAGED_UPGRADE_HEADROOM_BYTES.max(index_bytes.saturating_add(database_reserve))
}

fn validate_upgrade_storage_headroom(
    available_bytes: u64,
    required_bytes: u64,
) -> Result<(), ManagedDatabaseError> {
    if available_bytes < required_bytes {
        return Err(ManagedDatabaseError::UpgradeStorageHeadroom {
            available_bytes,
            required_bytes,
        });
    }
    Ok(())
}

fn validate_retained_upgrade_rollback(
    identity: &super::ManagedResourceIdentity,
    rollback: &ContainerInspection,
) -> Result<(), ManagedDatabaseError> {
    validate_owned_container(identity, rollback, false)
        .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?;
    if original_container_state(rollback)
        .map_err(|_| ManagedDatabaseError::UpgradeRecoveryFailed)?
        != OriginalContainerState::Stopped
    {
        return Err(ManagedDatabaseError::UpgradeRecoveryFailed);
    }
    Ok(())
}

fn restore_report(
    initialized: ManagedInitialization,
    schema: &cartograph_config::DatabaseSchema,
) -> ManagedRestoreReport {
    ManagedRestoreReport {
        capabilities: initialized.capabilities,
        migrations: initialized.migrations,
        schema: schema.as_str().to_owned(),
    }
}

fn upgrade_report(
    upgraded: bool,
    initialized: ManagedInitialization,
    schema: &cartograph_config::DatabaseSchema,
) -> ManagedUpgradeReport {
    ManagedUpgradeReport {
        upgraded,
        capabilities: initialized.capabilities,
        migrations: initialized.migrations,
        schema: schema.as_str().to_owned(),
    }
}

fn validate_new_backup_destination(destination: &Path) -> Result<(), ManagedDatabaseError> {
    if destination.file_name().is_none() || fs::symlink_metadata(destination).is_ok() {
        return Err(ManagedDatabaseError::BackupDestination);
    }
    destination
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or(ManagedDatabaseError::BackupDestination)?;
    Ok(())
}

fn verify_archive_file(path: &Path) -> Result<u64, ManagedDatabaseError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    if !metadata.file_type().is_file() || metadata.len() <= ARCHIVE_MAGIC_BYTES {
        return Err(ManagedDatabaseError::RestoreArchiveInvalid);
    }
    let mut file = open_archive(path)?;
    let mut magic = [0_u8; ARCHIVE_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    if &magic != ARCHIVE_MAGIC {
        return Err(ManagedDatabaseError::RestoreArchiveInvalid);
    }
    Ok(metadata.len())
}

fn stage_restore_archive(
    source: &Path,
    credential_path: &Path,
) -> Result<NamedTempFile, ManagedDatabaseError> {
    verify_archive_file(source)?;
    let mut source_file = open_archive(source)?;
    source_file
        .seek(SeekFrom::Start(0))
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    let parent = credential_path
        .parent()
        .ok_or(ManagedDatabaseError::CredentialPath)?;
    let mut staged =
        NamedTempFile::new_in(parent).map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    std::io::copy(&mut source_file, &mut staged)
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    staged
        .flush()
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)?;
    verify_archive_file(staged.path())?;
    Ok(staged)
}

#[cfg(unix)]
fn open_archive(path: &Path) -> Result<File, ManagedDatabaseError> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)
}

#[cfg(not(unix))]
fn open_archive(path: &Path) -> Result<File, ManagedDatabaseError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| ManagedDatabaseError::RestoreArchiveInvalid)
}

fn sync_parent(parent: &Path) -> Result<(), ManagedDatabaseError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ManagedDatabaseError::BackupDestination)
}

#[cfg(unix)]
fn set_private_archive_permissions(path: &Path) -> Result<(), ManagedDatabaseError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| ManagedDatabaseError::BackupDestination)
}

#[cfg(not(unix))]
fn set_private_archive_permissions(_path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

fn prefer_archive_cleanup<T>(
    result: Result<T, ManagedDatabaseError>,
    cleanup: Result<(), ManagedDatabaseError>,
) -> Result<T, ManagedDatabaseError> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(_)) => Err(ManagedDatabaseError::ArchiveCleanupFailed),
        (Err(error), _) => Err(error),
    }
}

async fn open_managed_database(
    credentials: &DatabaseCredentials,
    port: u16,
    schema: &cartograph_config::DatabaseSchema,
) -> Result<CartographDatabase, ManagedDatabaseError> {
    let url = credentials.database_url(port)?;
    let settings = DatabaseSettings::parse(url.expose_secret(), Some("2"), Some("10000"))
        .and_then(|settings| settings.with_schema(schema.as_str()))
        .map_err(|_| ManagedDatabaseError::CredentialFormat)?;
    let pool = connect(&settings)
        .await
        .map_err(|_| ManagedDatabaseError::DatabaseConnection)?;
    Ok(CartographDatabase::new(pool, settings.schema().clone()))
}

async fn read_bm25_health(
    database: &CartographDatabase,
) -> Result<ManagedDerivedIndexHealth, ManagedDatabaseError> {
    let schema = crate::database::quoted_schema(database.schema());
    let sql = format!(
        r#"WITH required AS (
                SELECT project_id, generation_id
                FROM {schema}."index_generations"
                WHERE state IN ('current', 'ready')
            ), healthy AS (
                SELECT required.project_id, required.generation_id
                FROM required
                INNER JOIN {schema}."generation_search_relations" AS relations
                  ON relations.project_id = required.project_id
                 AND relations.generation_id = required.generation_id
                INNER JOIN pg_catalog.pg_namespace AS namespaces
                  ON namespaces.nspname = $1
                INNER JOIN pg_catalog.pg_class AS tables
                  ON tables.relnamespace = namespaces.oid
                 AND tables.relname = 'search_g_'
                     || replace(required.generation_id::text, '-', '')
                 AND tables.relkind = 'r'
                 AND tables.relpersistence = 'p'
                INNER JOIN pg_catalog.pg_class AS index_relations
                  ON index_relations.relnamespace = namespaces.oid
                 AND index_relations.relname = tables.relname || '_bm25'
                INNER JOIN pg_catalog.pg_index AS indexes
                  ON indexes.indexrelid = index_relations.oid
                 AND indexes.indrelid = tables.oid
                 AND indexes.indisvalid
                 AND indexes.indisready
                INNER JOIN pg_catalog.pg_am AS methods
                  ON methods.oid = index_relations.relam
                 AND methods.amname IN ('paradedb', 'bm25')
            ), counts AS (
                SELECT (SELECT count(*) FROM required) AS required_count,
                       (SELECT count(*) FROM healthy) AS healthy_count
            )
            SELECT to_regclass($2) IS NOT NULL AS present,
                   required_count = healthy_count AS valid,
                   required_count = healthy_count AS ready,
                   EXISTS (SELECT 1 FROM pg_catalog.pg_am WHERE amname = 'paradedb')
                       AS bm25_access_method
            FROM counts"#
    );
    let row = query(AssertSqlSafe(sql))
        .bind(database.schema().as_str())
        .bind(format!(
            "{}.generation_search_relations",
            database.schema().as_str()
        ))
        .fetch_optional(&database.pool)
        .await
        .map_err(|_| ManagedDatabaseError::DatabaseCapabilityProbe)?;
    decode_bm25_health(row.as_ref())
}

fn decode_bm25_health(
    row: Option<&sqlx_postgres::PgRow>,
) -> Result<ManagedDerivedIndexHealth, ManagedDatabaseError> {
    let Some(row) = row else {
        return Ok(ManagedDerivedIndexHealth {
            availability: ManagedDerivedIndexAvailability {
                present: false,
                valid: false,
                ready: false,
            },
            bm25_access_method: false,
        });
    };
    Ok(ManagedDerivedIndexHealth {
        availability: ManagedDerivedIndexAvailability {
            present: row
                .try_get::<bool, _>(0)
                .map_err(|_| ManagedDatabaseError::DatabaseCapabilityProbe)?,
            valid: row
                .try_get::<bool, _>(1)
                .map_err(|_| ManagedDatabaseError::DatabaseCapabilityProbe)?,
            ready: row
                .try_get::<bool, _>(2)
                .map_err(|_| ManagedDatabaseError::DatabaseCapabilityProbe)?,
        },
        bm25_access_method: row
            .try_get::<bool, _>(3)
            .map_err(|_| ManagedDatabaseError::DatabaseCapabilityProbe)?,
    })
}

async fn repair_bm25_relations(
    database: &CartographDatabase,
    statement_timeout: std::time::Duration,
) -> Result<ManagedDerivedIndexHealth, ManagedDatabaseError> {
    database
        .maintain_generation_search_relations(Some(statement_timeout))
        .await
        .map_err(|_| ManagedDatabaseError::DerivedIndexUnhealthy)?;
    let health = read_bm25_health(database).await?;
    if !health.healthy() {
        return Err(ManagedDatabaseError::DerivedIndexUnhealthy);
    }
    Ok(health)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::ManagedDatabase;

    const LIVE_SCHEMA: &str = "cartograph_managed_maintenance_test";
    const UPGRADE_FIXTURE_SCHEMA: &str = "cartograph_managed_upgrade_fixture";
    const LIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
    const LIVE_PROJECT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const LIVE_GENERATION: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const LIVE_DOCUMENT: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const LIVE_DIGEST: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const HEADROOM_VOLUME_BYTES: u64 = 512 * 1024 * 1024;
    const HEADROOM_FILL_PATH: &str = "/var/lib/postgresql/cartograph-upgrade-headroom-fixture";
    const LEGACY_MANAGED_DATABASE_IMAGE: &str = concat!(
        "paradedb/paradedb:0.25.3@sha256:",
        "82d0c8bb0263c4320cb321591dd6831ecdd04b4b27328ef658358a9a8c383ac5"
    );
    const PREVIOUS_MANAGED_DATABASE_IMAGE: &str = concat!(
        "paradedb/paradedb:0.25.6@sha256:",
        "c5b04eba22497fa25de12265692e9578e309c2e2001d023ce6d08a17226c200a"
    );

    struct LiveDockerCleanup {
        container_name: String,
        rollback_name: String,
        volume_name: String,
        image: Option<String>,
    }

    impl Drop for LiveDockerCleanup {
        fn drop(&mut self) {
            for name in [&self.container_name, &self.rollback_name] {
                let _ = std::process::Command::new("docker")
                    .args(["container", "rm", "--force", "--volumes", name])
                    .output();
            }
            let _ = std::process::Command::new("docker")
                .args(["volume", "rm", "--force", &self.volume_name])
                .output();
            if let Some(image) = self.image.as_deref() {
                let _ = std::process::Command::new("docker")
                    .args(["image", "rm", "--force", image])
                    .output();
            }
        }
    }

    #[test]
    fn archive_validation_rejects_symlinks_empty_and_non_custom_files() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create archive fixture root: {error}"));
        let empty = directory.path().join("empty.dump");
        fs::write(&empty, b"")
            .unwrap_or_else(|error| panic!("could not write empty archive: {error}"));
        assert!(matches!(
            verify_archive_file(&empty),
            Err(ManagedDatabaseError::RestoreArchiveInvalid)
        ));
        let plain = directory.path().join("plain.dump");
        fs::write(&plain, b"not a postgres archive")
            .unwrap_or_else(|error| panic!("could not write plain archive: {error}"));
        assert!(matches!(
            verify_archive_file(&plain),
            Err(ManagedDatabaseError::RestoreArchiveInvalid)
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let archive = directory.path().join("archive.dump");
            fs::write(&archive, b"PGDMPpayload")
                .unwrap_or_else(|error| panic!("could not write archive fixture: {error}"));
            let link = directory.path().join("archive-link.dump");
            symlink(&archive, &link)
                .unwrap_or_else(|error| panic!("could not create archive symlink: {error}"));
            assert!(matches!(
                verify_archive_file(&link),
                Err(ManagedDatabaseError::RestoreArchiveInvalid)
            ));
        }
    }

    #[test]
    #[cfg(unix)]
    fn backup_archive_permissions_are_forced_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create archive permission root: {error}"));
        let archive = directory.path().join("archive.dump");
        fs::write(&archive, b"PGDMPpayload")
            .unwrap_or_else(|error| panic!("could not write archive permission fixture: {error}"));
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("could not loosen archive fixture: {error}"));

        set_private_archive_permissions(&archive)
            .unwrap_or_else(|error| panic!("could not harden archive permissions: {error}"));
        let mode = fs::metadata(&archive)
            .unwrap_or_else(|error| panic!("could not stat hardened archive: {error}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    #[cfg(not(unix))]
    fn backup_archive_creation_fails_without_private_acl_support() {
        assert_eq!(
            set_private_archive_permissions(Path::new("archive.dump")),
            Err(ManagedDatabaseError::CredentialAclUnsupported)
        );
    }

    #[test]
    fn derived_index_health_requires_every_catalog_invariant() {
        let healthy = ManagedDerivedIndexHealth {
            availability: ManagedDerivedIndexAvailability {
                present: true,
                valid: true,
                ready: true,
            },
            bm25_access_method: true,
        };
        assert!(healthy.healthy());
        assert!(
            !ManagedDerivedIndexHealth {
                availability: ManagedDerivedIndexAvailability {
                    valid: false,
                    ..healthy.availability
                },
                ..healthy
            }
            .healthy()
        );
        assert!(
            !ManagedDerivedIndexHealth {
                availability: ManagedDerivedIndexAvailability {
                    ready: false,
                    ..healthy.availability
                },
                ..healthy
            }
            .healthy()
        );
        assert!(
            !ManagedDerivedIndexHealth {
                bm25_access_method: false,
                ..healthy
            }
            .healthy()
        );
    }

    #[test]
    fn managed_upgrade_requires_bounded_data_volume_headroom_before_cutover() {
        let database_bytes = 8 * 1024 * 1024 * 1024;
        let index_bytes = 5 * 1024 * 1024 * 1024;
        let required = required_upgrade_storage_headroom(database_bytes, index_bytes);
        assert_eq!(
            required,
            index_bytes + database_bytes / super::super::MANAGED_UPGRADE_DATABASE_RESERVE_DIVISOR
        );
        assert_eq!(
            validate_upgrade_storage_headroom(required.saturating_sub(1), required),
            Err(ManagedDatabaseError::UpgradeStorageHeadroom {
                available_bytes: required.saturating_sub(1),
                required_bytes: required,
            })
        );
        assert_eq!(
            validate_upgrade_storage_headroom(required, required),
            Ok(())
        );
        assert_eq!(
            required_upgrade_storage_headroom(0, 0),
            super::super::MINIMUM_MANAGED_UPGRADE_HEADROOM_BYTES
        );
    }

    #[tokio::test]
    #[ignore = "starts a real pinned ParadeDB container on a constrained owned data volume"]
    async fn managed_upgrade_refuses_constrained_data_volume_before_cutover() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create headroom project: {error}"));
        let database = live_database(directory.path());
        let alias = format!(
            "cartograph-maintenance-headroom:{}",
            database.identity.project_hash
        );
        let _cleanup = live_cleanup(&database, Some(alias.clone()));
        create_constrained_headroom_volume(&database, &alias);
        initialize_headroom_database(&database, &alias).await;
        let (constrained_available, expected_required) = constrain_headroom_volume(&database).await;
        assert_headroom_upgrade_refused(
            &database,
            &alias,
            constrained_available,
            expected_required,
        )
        .await;
    }

    fn create_constrained_headroom_volume(database: &ManagedDatabase, alias: &str) {
        let project_label = format!("io.cartograph.project={}", database.identity.project_hash);
        let volume_size = format!("o=size={HEADROOM_VOLUME_BYTES}");
        assert_docker_success(&[
            "volume",
            "create",
            "--driver",
            "local",
            "--opt",
            "type=tmpfs",
            "--opt",
            "device=tmpfs",
            "--opt",
            &volume_size,
            "--label",
            "io.cartograph.managed=true",
            "--label",
            &project_label,
            &database.identity.volume_name,
        ]);
        assert_docker_success(&["image", "tag", super::super::MANAGED_DATABASE_IMAGE, alias]);
    }

    async fn initialize_headroom_database(database: &ManagedDatabase, alias: &str) {
        database
            .docker
            .ensure_available()
            .await
            .unwrap_or_else(|error| panic!("Docker is unavailable for headroom fixture: {error}"));
        let credentials = database
            .credentials
            .load_or_create()
            .unwrap_or_else(|error| panic!("could not create headroom credentials: {error}"));
        database
            .docker
            .containers()
            .create_container(&ContainerCreateSpec {
                identity: &database.identity,
                port: database.port,
                image: alias,
            })
            .await
            .unwrap_or_else(|error| panic!("could not create headroom container: {error}"));
        database
            .docker
            .containers()
            .install_password_file(
                &database.identity.container_name,
                database.credentials.path(),
            )
            .await
            .unwrap_or_else(|error| panic!("could not install headroom password: {error}"));
        database
            .docker
            .containers()
            .start_container(&database.identity.container_name, database.port)
            .await
            .unwrap_or_else(|error| panic!("could not start headroom container: {error}"));
        wait_for_owned_health(database, false).await;
        super::super::initialize_extensions(&database.docker, &database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not initialize headroom extensions: {error}"));
        let initialized = super::super::initialize_managed_database(
            &credentials.credentials,
            database.port,
            &database.schema,
        )
        .await
        .unwrap_or_else(|error| panic!("could not initialize headroom schema: {error}"));
        assert!(initialized.capabilities.ready);
    }

    async fn constrain_headroom_volume(database: &ManagedDatabase) -> (u64, u64) {
        let connection = open_test_database(database).await;
        let totals = connection
            .storage_totals(super::super::MANAGED_UPGRADE_STORAGE_INSPECTION_TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("could not inspect headroom storage: {error}"));
        connection.close().await;
        let expected_required =
            required_upgrade_storage_headroom(totals.database_bytes, totals.index_bytes);
        let available_before = database
            .lifecycle()
            .available_storage_bytes()
            .await
            .unwrap_or_else(|error| panic!("could not inspect initial headroom: {error}"));
        let target_available = super::super::MINIMUM_MANAGED_UPGRADE_HEADROOM_BYTES * 3 / 4;
        let fill_bytes = available_before
            .checked_sub(target_available)
            .filter(|bytes| *bytes > 0)
            .unwrap_or_else(|| panic!("bounded volume was already too small for the fixture"));
        let fill_bytes = fill_bytes.to_string();
        assert_docker_success(&[
            "container",
            "exec",
            &database.identity.container_name,
            "fallocate",
            "--length",
            &fill_bytes,
            HEADROOM_FILL_PATH,
        ]);
        let constrained_available = database
            .lifecycle()
            .available_storage_bytes()
            .await
            .unwrap_or_else(|error| panic!("could not inspect constrained headroom: {error}"));
        assert!(constrained_available < expected_required);
        (constrained_available, expected_required)
    }

    async fn assert_headroom_upgrade_refused(
        database: &ManagedDatabase,
        alias: &str,
        constrained_available: u64,
        expected_required: u64,
    ) {
        let rejected = database
            .maintenance()
            .upgrade(confirmation(database, ManagedDestructiveOperation::Upgrade))
            .await;
        assert!(matches!(
            rejected,
            Err(ManagedDatabaseError::UpgradeStorageHeadroom {
                available_bytes,
                required_bytes,
            }) if available_bytes == constrained_available && required_bytes == expected_required
        ));
        let primary = database
            .docker
            .containers()
            .inspect_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect refused primary: {error}"))
            .unwrap_or_else(|| panic!("refused primary was renamed or removed"));
        assert_eq!(primary.image, alias);
        assert_eq!(container_state(&primary), ManagedContainerState::Healthy);
        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            database.identity.container_name
        );
        let rollback = database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect refused rollback: {error}"));
        assert!(rollback.is_none());
        assert_docker_success(&[
            "container",
            "exec",
            &database.identity.container_name,
            "rm",
            "--force",
            HEADROOM_FILL_PATH,
        ]);
    }

    #[tokio::test]
    #[ignore = "starts a real digest-pinned ParadeDB container and exercises archive rollback"]
    async fn managed_backup_restore_rebuild_and_remove_are_verified() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create maintenance project: {error}"));
        let database = live_database(directory.path());
        let _cleanup = live_cleanup(&database, None);
        let started = database
            .lifecycle()
            .start()
            .await
            .unwrap_or_else(|error| panic!("could not start maintenance database: {error}"));
        assert!(started.capabilities.ready);

        let good_archive = directory.path().join("good.dump");
        install_marker_and_backup(&database, &good_archive).await;
        assert_verified_restore(&database, &good_archive).await;

        let incomplete_archive = directory.path().join("missing-derived-index.dump");
        prepare_incomplete_archive_and_rebuild(&database, &incomplete_archive).await;
        assert_incomplete_restore_rebuilds(&database, &incomplete_archive).await;

        let incompatible_archive = directory.path().join("invalid-migration-ledger.dump");
        prepare_incompatible_archive(&database, &incompatible_archive).await;
        assert_incompatible_restore_rolls_back(&database, &incompatible_archive).await;
        assert_malformed_restore_is_rejected(&database, directory.path()).await;
        assert_database_removal(&database).await;
    }

    async fn install_marker_and_backup(database: &ManagedDatabase, good_archive: &Path) {
        execute_test_sql(
            database,
            r#"CREATE TABLE "cartograph_managed_maintenance_test"."maintenance_marker" (
                value text NOT NULL
            )"#,
        )
        .await;
        execute_test_sql(
            database,
            r#"INSERT INTO "cartograph_managed_maintenance_test"."maintenance_marker" (value)
                VALUES ('from-backup')"#,
        )
        .await;
        install_managed_search_fixture(database).await;
        let backup = database
            .archives()
            .backup(good_archive)
            .await
            .unwrap_or_else(|error| panic!("managed backup failed: {error}"));
        assert!(backup.bytes > ARCHIVE_MAGIC_BYTES);
    }

    async fn install_managed_search_fixture(database: &ManagedDatabase) {
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{LIVE_SCHEMA}"."projects" (
                        project_id, root_identity, repository_fingerprint
                    ) VALUES ('{LIVE_PROJECT}'::uuid, 'managed/maintenance-fixture', '{LIVE_DIGEST}')"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{LIVE_SCHEMA}"."index_generations" (
                        project_id, generation_id, generation_sequence,
                        source_revision, state, worker_count, content_digest,
                        content_digest_version, ready_at, published_at
                    ) VALUES (
                        '{LIVE_PROJECT}'::uuid, '{LIVE_GENERATION}'::uuid, 1,
                        'managed-fixture', 'current', 1, '{LIVE_DIGEST}', 3,
                        clock_timestamp(), clock_timestamp()
                    )"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"UPDATE "{LIVE_SCHEMA}"."projects"
                    SET current_generation_id = '{LIVE_GENERATION}'::uuid
                    WHERE project_id = '{LIVE_PROJECT}'::uuid"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{LIVE_SCHEMA}"."search_documents" (
                        project_id, generation_id, document_id, path, language,
                        document_kind, qualified_name, code, natural_text, metadata
                    ) VALUES (
                        '{LIVE_PROJECT}'::uuid, '{LIVE_GENERATION}'::uuid, '{LIVE_DOCUMENT}'::uuid,
                        'src/managed.rs', 'rust', 'symbol', 'managedFixture',
                        'fn managed_fixture() {{}}', '', '{{}}'::jsonb
                    )"#
            ),
        )
        .await;
        let table = "search_g_bbbbbbbbbbbb4bbb8bbbbbbbbbbbbbbb";
        execute_test_sql(
            database,
            format!(
                r#"CREATE TABLE "{LIVE_SCHEMA}"."{table}" AS
                    SELECT * FROM "{LIVE_SCHEMA}"."search_documents"
                    WHERE generation_id = '{LIVE_GENERATION}'::uuid"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"CREATE INDEX "{table}_bm25" ON "{LIVE_SCHEMA}"."{table}"
                    USING bm25 (
                        id, project_id, generation_id, document_id, file_id, symbol_id,
                        path, language, document_kind,
                        (qualified_name::pdb.source_code), (code::pdb.source_code),
                        natural_text, metadata
                    ) WITH (key_field = 'id')"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{LIVE_SCHEMA}"."generation_search_relations" (
                        project_id, generation_id, document_count, content_digest
                    ) VALUES (
                        '{LIVE_PROJECT}'::uuid, '{LIVE_GENERATION}'::uuid, 1, '{LIVE_DIGEST}'
                    )"#
            ),
        )
        .await;
    }

    async fn assert_verified_restore(database: &ManagedDatabase, good_archive: &Path) {
        execute_test_sql(
            database,
            r#"UPDATE "cartograph_managed_maintenance_test"."maintenance_marker"
                SET value = 'mutated'"#,
        )
        .await;
        let restored = database
            .archives()
            .restore(
                good_archive,
                confirmation(database, ManagedDestructiveOperation::Restore),
            )
            .await
            .unwrap_or_else(|error| panic!("verified restore failed: {error}"));
        assert!(restored.capabilities.ready);
        assert_eq!(read_marker(database).await, "from-backup");
        assert!(live_health(database).await.healthy());
    }

    async fn prepare_incomplete_archive_and_rebuild(
        database: &ManagedDatabase,
        incomplete_archive: &Path,
    ) {
        execute_test_sql(
            database,
            r#"DROP INDEX "cartograph_managed_maintenance_test".
                "search_g_bbbbbbbbbbbb4bbb8bbbbbbbbbbbbbbb_bm25""#,
        )
        .await;
        assert!(!live_health(database).await.healthy());
        database
            .archives()
            .backup(incomplete_archive)
            .await
            .unwrap_or_else(|error| panic!("could not back up incomplete fixture: {error}"));
        let rebuilt = database
            .maintenance()
            .rebuild_derived_indexes(confirmation(
                database,
                ManagedDestructiveOperation::RebuildDerivedIndexes,
            ))
            .await
            .unwrap_or_else(|error| panic!("derived-index rebuild failed: {error}"));
        assert!(rebuilt.healthy());
    }

    async fn assert_incomplete_restore_rebuilds(
        database: &ManagedDatabase,
        incomplete_archive: &Path,
    ) {
        let restored = database
            .archives()
            .restore(
                incomplete_archive,
                confirmation(database, ManagedDestructiveOperation::Restore),
            )
            .await
            .unwrap_or_else(|error| panic!("incomplete derived-index restore failed: {error}"));
        assert!(restored.capabilities.ready);
        assert_eq!(read_marker(database).await, "from-backup");
        assert!(live_health(database).await.healthy());
    }

    async fn prepare_incompatible_archive(database: &ManagedDatabase, incompatible_archive: &Path) {
        let checksum = read_migration_checksum(database, 20).await;
        let invalid_checksum = "0".repeat(64);
        update_migration_checksum(database, 20, &invalid_checksum).await;
        database
            .archives()
            .backup(incompatible_archive)
            .await
            .unwrap_or_else(|error| panic!("could not back up incompatible fixture: {error}"));
        update_migration_checksum(database, 20, &checksum).await;
        execute_test_sql(
            database,
            r#"UPDATE "cartograph_managed_maintenance_test"."maintenance_marker"
                SET value = 'rollback-protected'"#,
        )
        .await;
    }

    async fn assert_incompatible_restore_rolls_back(
        database: &ManagedDatabase,
        incompatible_archive: &Path,
    ) {
        let failed_restore = database
            .archives()
            .restore(
                incompatible_archive,
                confirmation(database, ManagedDestructiveOperation::Restore),
            )
            .await;
        match failed_restore {
            Err(ManagedDatabaseError::RestoreVerificationFailed) => {}
            unexpected => panic!("incompatible archive returned the wrong result: {unexpected:?}"),
        }
        assert_eq!(read_marker(database).await, "rollback-protected");
        assert!(live_health(database).await.healthy());
    }

    async fn assert_malformed_restore_is_rejected(database: &ManagedDatabase, directory: &Path) {
        let malformed_archive = directory.join("malformed.dump");
        fs::write(&malformed_archive, b"PGDMPnot-a-real-custom-archive")
            .unwrap_or_else(|error| panic!("could not write malformed archive: {error}"));
        let malformed_restore = database
            .archives()
            .restore(
                &malformed_archive,
                confirmation(database, ManagedDestructiveOperation::Restore),
            )
            .await;
        assert!(matches!(
            malformed_restore,
            Err(ManagedDatabaseError::RestoreArchiveInvalid)
        ));
        assert_eq!(read_marker(database).await, "rollback-protected");
    }

    async fn assert_database_removal(database: &ManagedDatabase) {
        let removed = database
            .maintenance()
            .remove(confirmation(database, ManagedDestructiveOperation::Remove))
            .await
            .unwrap_or_else(|error| panic!("managed removal failed: {error}"));
        assert!(removed.container_removed);
        assert!(removed.volume_removed);
        assert!(removed.credentials_removed);
        assert!(!database.credentials.path().exists());
        let status = database
            .lifecycle()
            .status()
            .await
            .unwrap_or_else(|error| panic!("could not inspect removed database: {error}"));
        assert_eq!(status.state, ManagedContainerState::Missing);
    }

    #[tokio::test]
    #[ignore = "starts a real ParadeDB container and forces pinned-image upgrade rollback"]
    async fn managed_upgrade_resumes_after_interruption_between_rename_and_candidate() {
        assert_interrupted_upgrade_resumes(LEGACY_MANAGED_DATABASE_IMAGE, "0.25.3").await;
    }

    #[tokio::test]
    #[ignore = "starts the previous release's real ParadeDB image and resumes interrupted upgrade"]
    async fn managed_upgrade_from_previous_release_resumes_after_interrupted_rename() {
        assert_interrupted_upgrade_resumes(PREVIOUS_MANAGED_DATABASE_IMAGE, "0.25.6").await;
    }

    async fn assert_interrupted_upgrade_resumes(previous_image: &str, previous_version: &str) {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create upgrade project: {error}"));
        let database = live_database(directory.path());
        let old_image = format!(
            "cartograph-maintenance-upgrade-old:{}",
            database.identity.project_hash
        );
        let _cleanup = live_cleanup(&database, Some(old_image.clone()));
        assert_docker_success(&["image", "pull", previous_image]);
        assert_docker_success(&["image", "tag", previous_image, &old_image]);
        install_old_image_container(&database, &old_image).await;
        install_previous_image_data_fixture(&database, previous_version).await;
        let pre_upgrade_backup = directory.path().join("pre-upgrade.dump");
        let backup = database
            .archives()
            .backup(&pre_upgrade_backup)
            .await
            .unwrap_or_else(|error| panic!("could not back up previous image: {error}"));
        assert!(backup.bytes > ARCHIVE_MAGIC_BYTES);

        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            database.identity.container_name
        );
        database
            .docker
            .containers()
            .stop_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not stop pre-upgrade container: {error}"));
        database
            .docker
            .containers()
            .rename_container(&database.identity.container_name, &rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not simulate interrupted rename: {error}"));
        let missing_primary = database
            .docker
            .containers()
            .inspect_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect interrupted primary: {error}"));
        assert!(missing_primary.is_none());
        let interrupted_rollback = database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect interrupted rollback: {error}"))
            .unwrap_or_else(|| panic!("interrupted rollback is missing"));
        validate_retained_upgrade_rollback(&database.identity, &interrupted_rollback)
            .unwrap_or_else(|error| panic!("interrupted rollback is unsafe: {error}"));

        assert_pinned_upgrade_succeeds(&database).await;
    }

    #[tokio::test]
    #[ignore = "starts a real ParadeDB container and forces pinned-image upgrade rollback"]
    async fn managed_upgrade_recovers_before_and_after_extension_catalog_mutation() {
        assert_catalog_mutation_upgrade_recovers(LEGACY_MANAGED_DATABASE_IMAGE, "0.25.3").await;
    }

    #[tokio::test]
    #[ignore = "starts the previous release's real ParadeDB image and exercises upgrade recovery"]
    async fn managed_upgrade_from_previous_release_recovers_around_extension_catalog_mutation() {
        assert_catalog_mutation_upgrade_recovers(PREVIOUS_MANAGED_DATABASE_IMAGE, "0.25.6").await;
    }

    async fn assert_catalog_mutation_upgrade_recovers(
        previous_image: &str,
        previous_version: &str,
    ) {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("could not create upgrade project: {error}"));
        let database = live_database(directory.path());
        let old_image = format!(
            "cartograph-maintenance-upgrade-old:{}",
            database.identity.project_hash
        );
        let _cleanup = live_cleanup(&database, Some(old_image.clone()));
        assert_docker_success(&["image", "pull", previous_image]);
        assert_docker_success(&["image", "tag", previous_image, &old_image]);
        install_old_image_container(&database, &old_image).await;
        install_previous_image_data_fixture(&database, previous_version).await;
        install_malformed_upgrade_schema(&database).await;
        let pre_upgrade_backup = directory.path().join("pre-upgrade.dump");
        let backup = database
            .archives()
            .backup(&pre_upgrade_backup)
            .await
            .unwrap_or_else(|error| panic!("could not back up previous image: {error}"));
        assert!(backup.bytes > ARCHIVE_MAGIC_BYTES);
        assert_upgrade_failure_restores_old(
            &database,
            directory.path(),
            &old_image,
            previous_version,
        )
        .await;
        assert_post_extension_failure_retains_new(&database, &old_image).await;
        assert_pinned_upgrade_succeeds(&database).await;
    }

    async fn install_old_image_container(database: &ManagedDatabase, old_image: &str) {
        database
            .docker
            .ensure_available()
            .await
            .unwrap_or_else(|error| panic!("Docker is unavailable for upgrade fixture: {error}"));
        let volume_created = database
            .docker
            .volumes()
            .ensure_volume(&database.identity)
            .await
            .unwrap_or_else(|error| panic!("could not create old-image volume: {error}"));
        assert!(volume_created);
        database
            .credentials
            .load_or_create()
            .unwrap_or_else(|error| panic!("could not create upgrade credentials: {error}"));
        database
            .docker
            .containers()
            .create_container(&ContainerCreateSpec {
                identity: &database.identity,
                port: database.port,
                image: old_image,
            })
            .await
            .unwrap_or_else(|error| panic!("could not create old-image fixture: {error}"));
        database
            .docker
            .containers()
            .install_password_file(
                &database.identity.container_name,
                database.credentials.path(),
            )
            .await
            .unwrap_or_else(|error| panic!("could not install old-image password: {error}"));
        database
            .docker
            .containers()
            .start_container(&database.identity.container_name, database.port)
            .await
            .unwrap_or_else(|error| panic!("could not start old-image fixture: {error}"));
        wait_for_owned_health(database, false).await;
    }

    async fn install_previous_image_data_fixture(
        database: &ManagedDatabase,
        previous_version: &str,
    ) {
        execute_test_sql(database, "CREATE EXTENSION IF NOT EXISTS vector").await;
        execute_test_sql(database, "CREATE EXTENSION IF NOT EXISTS pg_search").await;
        assert_eq!(
            read_extension_version(database, "pg_search").await,
            previous_version
        );
        execute_test_sql(
            database,
            format!(r#"CREATE SCHEMA "{UPGRADE_FIXTURE_SCHEMA}""#),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"CREATE TABLE "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_marker" (
                        value text NOT NULL
                    )"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_marker" (value)
                    VALUES ('retained-across-upgrade')"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"CREATE TABLE "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_documents" (
                        id bigserial PRIMARY KEY,
                        document_id uuid NOT NULL,
                        qualified_name text NOT NULL
                    )"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"INSERT INTO "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_documents" (
                        document_id, qualified_name
                    ) VALUES ('{LIVE_DOCUMENT}'::uuid, 'managedUpgradeFixture')"#
            ),
        )
        .await;
        execute_test_sql(
            database,
            format!(
                r#"CREATE INDEX "upgrade_documents_bm25"
                    ON "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_documents"
                    USING bm25 (id, (qualified_name::pdb.source_code))
                    WITH (key_field = 'id')"#
            ),
        )
        .await;
    }

    async fn install_malformed_upgrade_schema(database: &ManagedDatabase) {
        execute_test_sql(database, format!(r#"CREATE SCHEMA "{LIVE_SCHEMA}""#)).await;
        execute_test_sql(
            database,
            format!(
                r#"CREATE TABLE "{LIVE_SCHEMA}"."schema_migrations" (
                        incompatible integer NOT NULL
                    )"#
            ),
        )
        .await;
    }

    async fn assert_upgrade_failure_restores_old(
        database: &ManagedDatabase,
        directory: &Path,
        old_image: &str,
        previous_version: &str,
    ) {
        let zero_timeout = live_database_with_port(directory, database.port)
            .with_startup_timeout(std::time::Duration::ZERO);
        let failed_upgrade = zero_timeout
            .maintenance()
            .upgrade(confirmation(
                &zero_timeout,
                ManagedDestructiveOperation::Upgrade,
            ))
            .await;
        assert!(matches!(
            failed_upgrade,
            Err(ManagedDatabaseError::DatabaseStartupTimeout)
        ));
        wait_for_owned_health(database, false).await;
        let rolled_back = database
            .docker
            .containers()
            .inspect_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect rollback container: {error}"))
            .unwrap_or_else(|| panic!("rollback container is missing"));
        assert_eq!(rolled_back.image, old_image);
        assert_eq!(
            read_extension_version(database, "pg_search").await,
            previous_version
        );
        assert_eq!(
            read_upgrade_marker(database).await,
            "retained-across-upgrade"
        );
        assert_upgrade_search_hit(database).await;
        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            database.identity.container_name
        );
        let retained_rollback = database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect rollback slot: {error}"));
        assert!(retained_rollback.is_none());
    }

    async fn assert_post_extension_failure_retains_new(
        database: &ManagedDatabase,
        old_image: &str,
    ) {
        let failed_upgrade = database
            .maintenance()
            .upgrade(confirmation(database, ManagedDestructiveOperation::Upgrade))
            .await;
        assert!(matches!(
            failed_upgrade,
            Err(ManagedDatabaseError::SchemaMigration)
        ));

        wait_for_owned_health(database, true).await;
        let candidate = database
            .docker
            .containers()
            .inspect_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect retained candidate: {error}"))
            .unwrap_or_else(|| panic!("retained candidate is missing"));
        assert_eq!(candidate.image, super::super::MANAGED_DATABASE_IMAGE);
        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            database.identity.container_name
        );
        let rollback = database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect retained rollback: {error}"))
            .unwrap_or_else(|| panic!("retained rollback is missing"));
        assert_eq!(rollback.image, old_image);
        assert_eq!(
            original_container_state(&rollback)
                .unwrap_or_else(|error| panic!("retained rollback state is invalid: {error}")),
            OriginalContainerState::Stopped
        );
        assert_eq!(
            read_extension_version(database, "pg_search").await,
            "0.25.7"
        );
        let connection = open_test_database(database).await;
        let capabilities = connection
            .capability_report()
            .await
            .unwrap_or_else(|error| panic!("retained candidate capability probe failed: {error}"));
        assert!(capabilities.ready);
        connection.close().await;
        assert_eq!(
            read_upgrade_marker(database).await,
            "retained-across-upgrade"
        );
        assert_upgrade_search_hit(database).await;
        execute_test_sql(database, format!(r#"DROP SCHEMA "{LIVE_SCHEMA}" CASCADE"#)).await;
    }

    async fn assert_pinned_upgrade_succeeds(database: &ManagedDatabase) {
        let upgraded = database
            .maintenance()
            .upgrade(confirmation(database, ManagedDestructiveOperation::Upgrade))
            .await
            .unwrap_or_else(|error| panic!("pinned-image upgrade failed: {error}"));
        assert!(upgraded.upgraded);
        assert!(upgraded.capabilities.ready);
        assert_eq!(
            upgraded.capabilities.pg_search_version.as_deref(),
            Some(crate::capabilities::SUPPORTED_PG_SEARCH_VERSION)
        );
        assert_eq!(
            upgraded.capabilities.pgvector_version.as_deref(),
            Some(crate::capabilities::MANAGED_PGVECTOR_VERSION)
        );
        let inspection = database
            .docker
            .containers()
            .inspect_container(&database.identity.container_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect upgraded container: {error}"))
            .unwrap_or_else(|| panic!("upgraded container is missing"));
        assert_eq!(inspection.image, super::super::MANAGED_DATABASE_IMAGE);
        let rollback_name = format!(
            "{}{UPGRADE_ROLLBACK_SUFFIX}",
            database.identity.container_name
        );
        let rollback = database
            .docker
            .containers()
            .inspect_container(&rollback_name)
            .await
            .unwrap_or_else(|error| panic!("could not inspect completed rollback slot: {error}"));
        assert!(rollback.is_none());
        assert_eq!(
            read_upgrade_marker(database).await,
            "retained-across-upgrade"
        );
        assert_upgrade_search_hit(database).await;

        let repeated = database
            .maintenance()
            .upgrade(confirmation(database, ManagedDestructiveOperation::Upgrade))
            .await
            .unwrap_or_else(|error| panic!("repeated pinned-image upgrade failed: {error}"));
        assert!(!repeated.upgraded);
        assert!(repeated.capabilities.ready);
        assert_eq!(
            read_upgrade_marker(database).await,
            "retained-across-upgrade"
        );
        assert_upgrade_search_hit(database).await;

        database
            .maintenance()
            .remove(confirmation(database, ManagedDestructiveOperation::Remove))
            .await
            .unwrap_or_else(|error| panic!("could not remove upgrade fixture: {error}"));
    }

    fn live_database(project_root: &Path) -> ManagedDatabase {
        live_database_with_port(project_root, available_loopback_port())
    }

    fn live_database_with_port(project_root: &Path, port: u16) -> ManagedDatabase {
        let schema = cartograph_config::DatabaseSchema::parse(LIVE_SCHEMA)
            .unwrap_or_else(|error| panic!("live maintenance schema is invalid: {error}"));
        super::super::managed_database_with_schema(project_root, port, schema)
            .unwrap_or_else(|error| panic!("could not build live maintenance manager: {error}"))
            .with_startup_timeout(LIVE_TIMEOUT)
            .with_maintenance_timeout(LIVE_TIMEOUT)
    }

    fn available_loopback_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("could not reserve maintenance port: {error}"));
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("could not inspect maintenance port: {error}"))
            .port()
    }

    fn live_cleanup(database: &ManagedDatabase, image: Option<String>) -> LiveDockerCleanup {
        LiveDockerCleanup {
            container_name: database.identity.container_name.clone(),
            rollback_name: format!(
                "{}{UPGRADE_ROLLBACK_SUFFIX}",
                database.identity.container_name
            ),
            volume_name: database.identity.volume_name.clone(),
            image,
        }
    }

    fn confirmation(
        database: &ManagedDatabase,
        operation: ManagedDestructiveOperation,
    ) -> ManagedDestructiveConfirmation {
        database
            .confirm_destructive_operation(operation, operation.confirmation_phrase())
            .unwrap_or_else(|error| panic!("could not confirm destructive operation: {error}"))
    }

    async fn execute_test_sql(database: &ManagedDatabase, statement: impl Into<String>) {
        let credentials = database
            .credentials
            .load()
            .unwrap_or_else(|error| panic!("could not load maintenance credentials: {error}"));
        let connection = open_managed_database(&credentials, database.port, &database.schema)
            .await
            .unwrap_or_else(|error| panic!("could not open maintenance database: {error}"));
        query(AssertSqlSafe(statement.into()))
            .execute(&connection.pool)
            .await
            .unwrap_or_else(|error| panic!("maintenance fixture statement failed: {error}"));
        connection.pool.close().await;
    }

    async fn open_test_database(database: &ManagedDatabase) -> CartographDatabase {
        let credentials = database
            .credentials
            .load()
            .unwrap_or_else(|error| panic!("could not load test database credentials: {error}"));
        open_managed_database(&credentials, database.port, &database.schema)
            .await
            .unwrap_or_else(|error| panic!("could not open test database: {error}"))
    }

    async fn read_extension_version(database: &ManagedDatabase, extension: &str) -> String {
        let connection = open_test_database(database).await;
        let row = query("SELECT extversion FROM pg_extension WHERE extname = $1")
            .bind(extension)
            .fetch_one(&connection.pool)
            .await
            .unwrap_or_else(|error| panic!("could not read extension version: {error}"));
        let version = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("extension version was invalid: {error}"));
        connection.close().await;
        version
    }

    async fn assert_upgrade_search_hit(database: &ManagedDatabase) {
        let connection = open_test_database(database).await;
        let sql = format!(
            r#"SELECT documents.document_id::text
                FROM "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_documents" AS documents
                WHERE documents.qualified_name ||| $1
                ORDER BY pdb.score(documents.id) DESC, documents.id ASC
                LIMIT 1"#
        );
        let row = query(AssertSqlSafe(sql))
            .bind("managedUpgradeFixture")
            .fetch_one(&connection.pool)
            .await
            .unwrap_or_else(|error| panic!("upgrade BM25 query failed: {error}"));
        let document = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("upgrade BM25 document was invalid: {error}"));
        assert_eq!(document, LIVE_DOCUMENT);
        connection.close().await;
    }

    async fn read_upgrade_marker(database: &ManagedDatabase) -> String {
        let connection = open_test_database(database).await;
        let sql = format!(r#"SELECT value FROM "{UPGRADE_FIXTURE_SCHEMA}"."upgrade_marker""#);
        let row = query(AssertSqlSafe(sql))
            .fetch_one(&connection.pool)
            .await
            .unwrap_or_else(|error| panic!("could not read upgrade marker: {error}"));
        let value = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("upgrade marker value was invalid: {error}"));
        connection.close().await;
        value
    }

    async fn read_marker(database: &ManagedDatabase) -> String {
        let credentials = database
            .credentials
            .load()
            .unwrap_or_else(|error| panic!("could not load marker credentials: {error}"));
        let connection = open_managed_database(&credentials, database.port, &database.schema)
            .await
            .unwrap_or_else(|error| panic!("could not open marker database: {error}"));
        let row = query(
            r#"SELECT value
                FROM "cartograph_managed_maintenance_test"."maintenance_marker""#,
        )
        .fetch_one(&connection.pool)
        .await
        .unwrap_or_else(|error| panic!("could not read marker: {error}"));
        let value = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("marker value was invalid: {error}"));
        connection.pool.close().await;
        value
    }

    async fn read_migration_checksum(database: &ManagedDatabase, version: i64) -> String {
        let credentials = database
            .credentials
            .load()
            .unwrap_or_else(|error| panic!("could not load ledger credentials: {error}"));
        let connection = open_managed_database(&credentials, database.port, &database.schema)
            .await
            .unwrap_or_else(|error| panic!("could not open ledger database: {error}"));
        let row = query(
            r#"SELECT checksum
                FROM "cartograph_managed_maintenance_test"."schema_migrations"
                WHERE version = $1"#,
        )
        .bind(version)
        .fetch_one(&connection.pool)
        .await
        .unwrap_or_else(|error| panic!("could not read migration checksum: {error}"));
        let checksum = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("migration checksum was invalid: {error}"));
        connection.pool.close().await;
        checksum
    }

    async fn update_migration_checksum(database: &ManagedDatabase, version: i64, checksum: &str) {
        let credentials = database
            .credentials
            .load()
            .unwrap_or_else(|error| panic!("could not load ledger credentials: {error}"));
        let connection = open_managed_database(&credentials, database.port, &database.schema)
            .await
            .unwrap_or_else(|error| panic!("could not open ledger database: {error}"));
        let updated = query(
            r#"UPDATE "cartograph_managed_maintenance_test"."schema_migrations"
                SET checksum = $2
                WHERE version = $1"#,
        )
        .bind(version)
        .bind(checksum)
        .execute(&connection.pool)
        .await
        .unwrap_or_else(|error| panic!("could not update migration checksum: {error}"));
        assert_eq!(updated.rows_affected(), 1);
        connection.pool.close().await;
    }

    async fn live_health(database: &ManagedDatabase) -> ManagedDerivedIndexHealth {
        database
            .maintenance()
            .derived_index_health()
            .await
            .unwrap_or_else(|error| panic!("could not read derived-index health: {error}"))
    }

    async fn wait_for_owned_health(database: &ManagedDatabase, require_supported_image: bool) {
        let waited = timeout(LIVE_TIMEOUT, async {
            loop {
                let inspection = database
                    .docker
                    .containers()
                    .inspect_container(&database.identity.container_name)
                    .await
                    .unwrap_or_else(|error| panic!("could not inspect live fixture: {error}"))
                    .unwrap_or_else(|| panic!("live fixture container is missing"));
                validate_owned_container(&database.identity, &inspection, require_supported_image)
                    .unwrap_or_else(|error| panic!("live fixture ownership changed: {error}"));
                match container_state(&inspection) {
                    ManagedContainerState::Healthy => break,
                    ManagedContainerState::Created
                    | ManagedContainerState::Starting
                    | ManagedContainerState::Restarting => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    state => panic!("live fixture entered unexpected state: {state:?}"),
                }
            }
        })
        .await;
        assert!(waited.is_ok(), "live fixture health timed out");
    }

    fn assert_docker_success(arguments: &[&str]) {
        let output = std::process::Command::new("docker")
            .args(arguments)
            .output()
            .unwrap_or_else(|error| panic!("could not run Docker fixture command: {error}"));
        assert!(
            output.status.success(),
            "Docker fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
