use std::{
    ffi::OsString, net::IpAddr, path::Path, process::ExitStatus, sync::OnceLock, time::Duration,
};

use serde::Deserialize;
use tokio::{process::Command, time::timeout};

use crate::capabilities::{MANAGED_PGVECTOR_VERSION, SUPPORTED_PG_SEARCH_VERSION};

use super::{
    MANAGED_DATABASE_MEMORY_LIMIT_BYTES, MANAGED_DATABASE_MEMORY_RESERVATION_BYTES,
    MANAGED_DATABASE_NANO_CPUS, MANAGED_DATABASE_PIDS_LIMIT, MANAGED_DATABASE_SHARED_MEMORY_BYTES,
    ManagedDatabaseError, ManagedResourceIdentity,
};

const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DOCKER_IMAGE_PULL_TIMEOUT: Duration = Duration::from_mins(15);
const DOCKER_ARCHIVE_TIMEOUT: Duration = Duration::from_mins(15);
const CONTAINER_ARCHIVE_TIMEOUT: &str = "14m";
const CONTAINER_ARCHIVE_KILL_AFTER: &str = "10s";
const NORMAL_STOP_POLICY: StopPolicy = StopPolicy {
    grace_seconds: "30",
    command_timeout: Duration::from_secs(45),
};
const ROLLBACK_STOP_POLICY: StopPolicy = StopPolicy {
    grace_seconds: "3",
    command_timeout: DOCKER_COMMAND_TIMEOUT,
};
const DATABASE_USER: &str = "cartograph";
const DATABASE_NAME: &str = "cartograph";
const MAINTENANCE_DATABASE_NAME: &str = "postgres";
const CONTAINER_PASSWORD_PATH: &str = "/tmp/cartograph-postgres-password";
const DATABASE_DATA_PATH: &str = "/var/lib/postgresql";
const INSPECT_TEMPLATE: &str = concat!(
    "{{index .Config.Labels \"io.cartograph.managed\"}}\t",
    "{{index .Config.Labels \"io.cartograph.project\"}}\t",
    "{{.State.Status}}\t",
    "{{with index .State \"Health\"}}{{.Status}}{{else}}none{{end}}\t",
    "{{.Config.Image}}\t",
    "{{with index .HostConfig.PortBindings \"5432/tcp\"}}",
    "{{(index . 0).HostIp}}\t{{(index . 0).HostPort}}",
    "{{else}}none\tnone{{end}}\t",
    "{{.HostConfig.ShmSize}}\t",
    "{{.HostConfig.Memory}}\t",
    "{{.HostConfig.MemoryReservation}}\t",
    "{{.HostConfig.NanoCpus}}\t",
    "{{with .HostConfig.PidsLimit}}{{.}}{{else}}0{{end}}\t",
    "{{json .Mounts}}\t",
    "{{json .Config.Cmd}}"
);
const VOLUME_INSPECT_TEMPLATE: &str = concat!(
    "{{index .Labels \"io.cartograph.managed\"}}\t",
    "{{index .Labels \"io.cartograph.project\"}}"
);

pub(super) struct DockerCli {
    runner: DockerCommandRunner,
}

pub(super) struct DockerContainerCli<'a> {
    runner: &'a DockerCommandRunner,
}

pub(super) struct DockerArchiveCli<'a> {
    runner: &'a DockerCommandRunner,
}

pub(super) struct DockerVolumeCli<'a> {
    runner: &'a DockerCommandRunner,
}

struct DockerCommandRunner {
    command: OsString,
    endpoint: OnceLock<String>,
}

pub(super) struct ContainerInspection {
    pub(super) managed: bool,
    pub(super) project_hash: String,
    pub(super) process_state: String,
    pub(super) health_state: String,
    pub(super) image: String,
    pub(super) host_ip: String,
    pub(super) host_port: String,
    pub(super) shared_memory_bytes: u64,
    pub(super) memory_limit_bytes: u64,
    pub(super) memory_reservation_bytes: u64,
    pub(super) nano_cpus: u64,
    pub(super) pids_limit: i64,
    pub(super) data_mount: Option<ContainerMountInspection>,
    /// Container command, including the PostgreSQL server settings.
    pub(super) command: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct ContainerMountInspection {
    #[serde(rename = "Type")]
    mount_type: String,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Destination")]
    destination: String,
    #[serde(rename = "RW")]
    read_write: bool,
}

pub(super) struct ContainerCreateSpec<'a> {
    pub(super) identity: &'a ManagedResourceIdentity,
    pub(super) port: u16,
    pub(super) image: &'a str,
    /// Container command; production passes [`postgres_command`].
    pub(super) command: &'a [String],
}

/// PostgreSQL server settings on the managed container's command line.
///
/// WAL left after a busy period tracks `max_wal_size`, and an idle database
/// does not checkpoint it away, so the cap is also each project's steady-state
/// WAL footprint. 2 GB halves the earlier 4 GB ceiling; lz4 compression offsets
/// the extra full-page images a smaller cap causes (15-25% less WAL once
/// checkpoints fall inside an index run).
const POSTGRES_SETTINGS: [&str; 13] = [
    "shared_buffers=256MB",
    "effective_cache_size=1GB",
    "maintenance_work_mem=128MB",
    "autovacuum_work_mem=64MB",
    "work_mem=4MB",
    "checkpoint_timeout=15min",
    "max_wal_size=2GB",
    "min_wal_size=256MB",
    "wal_compression=lz4",
    // The application pool is capped at 64 per process. Leave regular-backend
    // headroom for a concurrent MCP/CLI process and PostgreSQL's reserved slots.
    "max_connections=96",
    "max_worker_processes=16",
    "max_parallel_workers=4",
    "max_parallel_maintenance_workers=2",
];

/// The command a supported managed container runs.
pub(super) fn postgres_command() -> Vec<String> {
    std::iter::once("postgres".to_owned())
        .chain(
            POSTGRES_SETTINGS
                .iter()
                .flat_map(|setting| ["-c".to_owned(), (*setting).to_owned()]),
        )
        .collect()
}

/// The command v2.1.32 created containers with, before the WAL change.
#[cfg(test)]
pub(super) const LEGACY_POSTGRES_COMMAND: [&str; 25] = [
    "postgres",
    "-c",
    "shared_buffers=256MB",
    "-c",
    "effective_cache_size=1GB",
    "-c",
    "maintenance_work_mem=128MB",
    "-c",
    "autovacuum_work_mem=64MB",
    "-c",
    "work_mem=4MB",
    "-c",
    "checkpoint_timeout=15min",
    "-c",
    "max_wal_size=4GB",
    "-c",
    "min_wal_size=512MB",
    "-c",
    "max_connections=96",
    "-c",
    "max_worker_processes=16",
    "-c",
    "max_parallel_workers=4",
    "-c",
    "max_parallel_maintenance_workers=2",
];

/// Whether the container runs the current PostgreSQL settings. Older
/// containers keep working; `db upgrade` recreates them with these settings.
pub(super) fn has_current_postgres_settings(inspection: &ContainerInspection) -> bool {
    inspection.command == postgres_command()
}

struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct CheckedDockerCommand {
    operation: &'static str,
    args: Vec<OsString>,
    command_timeout: Duration,
}

#[derive(Clone, Copy)]
pub(super) enum DatabaseArchiveOperation {
    Create,
    Verify,
    Restore,
}

pub(super) struct DatabaseArchiveRequest<'a> {
    name: &'a str,
    container_path: &'a str,
    operation: DatabaseArchiveOperation,
}

impl<'a> DatabaseArchiveRequest<'a> {
    pub(super) const fn new(
        name: &'a str,
        container_path: &'a str,
        operation: DatabaseArchiveOperation,
    ) -> Self {
        Self {
            name,
            container_path,
            operation,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ContainerArchivePath<'a> {
    pub(super) name: &'a str,
    pub(super) path: &'a str,
}

#[derive(Clone, Copy)]
enum ContainerCopyDirection {
    FromContainer,
    ToContainer,
}

struct ContainerArchiveCopy<'a> {
    container: ContainerArchivePath<'a>,
    host_path: &'a Path,
    direction: ContainerCopyDirection,
}

#[derive(Clone, Copy)]
struct StopPolicy {
    grace_seconds: &'static str,
    command_timeout: Duration,
}

impl DockerCli {
    pub(super) fn new() -> Self {
        Self {
            runner: DockerCommandRunner::new(),
        }
    }

    pub(super) async fn ensure_available(&self) -> Result<(), ManagedDatabaseError> {
        self.runner.pin_local_endpoint().await?;
        self.runner
            .checked(
                "inspect Docker engine",
                ["info", "--format", "{{.ServerVersion}}"],
            )
            .await
            .map(|_| ())
    }

    pub(super) async fn ensure_image(&self, image: &str) -> Result<(), ManagedDatabaseError> {
        let inspection = self.runner.execute(["image", "inspect", image]).await?;
        if inspection.status.success() {
            return Ok(());
        }
        if !inspection
            .stderr
            .to_ascii_lowercase()
            .contains("no such image")
        {
            return Err(ManagedDatabaseError::DockerOperation {
                operation: "inspect managed database image",
            });
        }

        let pull = self
            .runner
            .execute_os_with_timeout(
                vec![
                    OsString::from("image"),
                    OsString::from("pull"),
                    OsString::from(image),
                ],
                DOCKER_IMAGE_PULL_TIMEOUT,
            )
            .await
            .map_err(|error| match error {
                ManagedDatabaseError::DockerTimeout => ManagedDatabaseError::DockerImagePullTimeout,
                other => other,
            })?;
        if !pull.status.success() {
            return Err(ManagedDatabaseError::DockerOperation {
                operation: "pull managed database image",
            });
        }
        self.runner
            .checked("verify managed database image", ["image", "inspect", image])
            .await
            .map(|_| ())
    }

    pub(super) const fn containers(&self) -> DockerContainerCli<'_> {
        DockerContainerCli {
            runner: &self.runner,
        }
    }

    pub(super) const fn archives(&self) -> DockerArchiveCli<'_> {
        DockerArchiveCli {
            runner: &self.runner,
        }
    }

    pub(super) const fn volumes(&self) -> DockerVolumeCli<'_> {
        DockerVolumeCli {
            runner: &self.runner,
        }
    }
}

impl DockerContainerCli<'_> {
    pub(super) async fn inspect_container(
        &self,
        name: &str,
    ) -> Result<Option<ContainerInspection>, ManagedDatabaseError> {
        let output = self
            .runner
            .execute(["container", "inspect", "--format", INSPECT_TEMPLATE, name])
            .await?;
        if !output.status.success() {
            if output.stderr.to_ascii_lowercase().contains("no such") {
                return Ok(None);
            }
            return Err(ManagedDatabaseError::DockerOperation {
                operation: "inspect managed container",
            });
        }
        parse_container_inspection(trim_line_end(&output.stdout)).map(Some)
    }

    pub(super) async fn available_data_bytes(
        &self,
        name: &str,
    ) -> Result<u64, ManagedDatabaseError> {
        let output = self
            .runner
            .checked(
                "inspect managed database free space",
                ["container", "exec", name, "df", "-Pk", DATABASE_DATA_PATH],
            )
            .await?;
        let line = output.lines().rfind(|line| !line.trim().is_empty()).ok_or(
            ManagedDatabaseError::DockerOperation {
                operation: "decode managed database free space",
            },
        )?;
        let available_kib = line
            .split_whitespace()
            .nth(3)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(ManagedDatabaseError::DockerOperation {
                operation: "decode managed database free space",
            })?;
        available_kib
            .checked_mul(1024)
            .ok_or(ManagedDatabaseError::DockerOperation {
                operation: "decode managed database free space",
            })
    }
}

impl DockerVolumeCli<'_> {
    pub(super) async fn ensure_volume(
        &self,
        identity: &ManagedResourceIdentity,
    ) -> Result<bool, ManagedDatabaseError> {
        if let Some(existing) = inspect_volume_labels(self, identity).await? {
            validate_volume_ownership(&existing, identity)?;
            return Ok(false);
        }

        self.runner
            .checked(
                "create managed volume",
                [
                    "volume",
                    "create",
                    "--label",
                    "io.cartograph.managed=true",
                    "--label",
                    &format!("io.cartograph.project={}", identity.project_hash),
                    &identity.volume_name,
                ],
            )
            .await?;
        verify_volume(self, identity).await?;
        Ok(true)
    }
}

impl DockerContainerCli<'_> {
    pub(super) async fn create_container(
        &self,
        spec: &ContainerCreateSpec<'_>,
    ) -> Result<(), ManagedDatabaseError> {
        let args = create_container_arguments(spec);
        let output = self.runner.execute_os(args).await?;
        if output.status.success() {
            return Ok(());
        }
        let detail = output.stderr.to_ascii_lowercase();
        if detail.contains("port is already allocated")
            || detail.contains("address already in use")
            || detail.contains("failed programming external connectivity")
        {
            return Err(ManagedDatabaseError::PortUnavailable { port: spec.port });
        }
        Err(ManagedDatabaseError::DockerOperation {
            operation: "create managed container",
        })
    }

    pub(super) async fn install_password_file(
        &self,
        name: &str,
        password_file: &Path,
    ) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked_os(
                "install managed database password",
                password_copy_arguments(name, password_file),
            )
            .await
            .map(|_| ())
    }

    pub(super) async fn start_container(
        &self,
        name: &str,
        port: u16,
    ) -> Result<(), ManagedDatabaseError> {
        let output = self.runner.execute(["container", "start", name]).await?;
        if output.status.success() {
            return Ok(());
        }
        let detail = output.stderr.to_ascii_lowercase();
        if detail.contains("port is already allocated")
            || detail.contains("address already in use")
            || detail.contains("failed programming external connectivity")
        {
            return Err(ManagedDatabaseError::PortUnavailable { port });
        }
        Err(ManagedDatabaseError::DockerOperation {
            operation: "start managed container",
        })
    }

    pub(super) async fn pause_container(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked("pause managed container", ["container", "pause", name])
            .await
            .map(|_| ())
    }

    pub(super) async fn unpause_container(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked("unpause managed container", ["container", "unpause", name])
            .await
            .map(|_| ())
    }

    pub(super) async fn stop_container(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        stop_container_with_grace(self.runner, name, NORMAL_STOP_POLICY).await
    }

    pub(super) async fn rollback_container(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        stop_container_with_grace(self.runner, name, ROLLBACK_STOP_POLICY).await
    }

    pub(super) async fn verify_loopback_port(
        &self,
        name: &str,
        port: u16,
    ) -> Result<(), ManagedDatabaseError> {
        let published = self
            .runner
            .checked("inspect managed port", ["port", name, "5432/tcp"])
            .await?;
        if published.trim() != format!("127.0.0.1:{port}") {
            return Err(ManagedDatabaseError::UnsafePortPublication);
        }
        Ok(())
    }

    pub(super) async fn logs(&self, name: &str, tail: u16) -> Result<String, ManagedDatabaseError> {
        let output = self
            .runner
            .execute(["logs", "--tail", &tail.to_string(), name])
            .await?;
        if !output.status.success() {
            return Err(ManagedDatabaseError::DockerOperation {
                operation: "read managed container logs",
            });
        }
        Ok(join_output_streams(&output.stdout, &output.stderr))
    }
}

impl DockerVolumeCli<'_> {
    pub(super) async fn volume_exists_owned(
        &self,
        identity: &ManagedResourceIdentity,
    ) -> Result<bool, ManagedDatabaseError> {
        let Some(labels) = inspect_volume_labels(self, identity).await? else {
            return Ok(false);
        };
        validate_volume_ownership(&labels, identity)?;
        Ok(true)
    }
}

impl DockerArchiveCli<'_> {
    pub(super) async fn copy_from_container(
        &self,
        source: ContainerArchivePath<'_>,
        host_path: &Path,
    ) -> Result<(), ManagedDatabaseError> {
        self.copy_container_archive(ContainerArchiveCopy {
            container: source,
            host_path,
            direction: ContainerCopyDirection::FromContainer,
        })
        .await
    }

    pub(super) async fn copy_to_container(
        &self,
        host_path: &Path,
        target: ContainerArchivePath<'_>,
    ) -> Result<(), ManagedDatabaseError> {
        self.copy_container_archive(ContainerArchiveCopy {
            container: target,
            host_path,
            direction: ContainerCopyDirection::ToContainer,
        })
        .await
    }

    pub(super) async fn database_archive(
        &self,
        request: DatabaseArchiveRequest<'_>,
    ) -> Result<(), ManagedDatabaseError> {
        if matches!(request.operation, DatabaseArchiveOperation::Restore) {
            self.runner
                .checked_os_with_timeout(CheckedDockerCommand {
                    operation: "reset managed database before restore",
                    args: database_reset_arguments(request.name),
                    command_timeout: DOCKER_ARCHIVE_TIMEOUT,
                })
                .await
                .map_err(|_| ManagedDatabaseError::RestoreFailed)?;
        }
        let (operation, args) = match request.operation {
            DatabaseArchiveOperation::Create => (
                "create managed database archive",
                database_backup_arguments(request.name, request.container_path),
            ),
            DatabaseArchiveOperation::Verify => (
                "verify managed database archive",
                database_archive_list_arguments(request.name, request.container_path),
            ),
            DatabaseArchiveOperation::Restore => (
                "restore managed database archive",
                database_restore_arguments(request.name, request.container_path),
            ),
        };
        self.runner
            .checked_os_with_timeout(CheckedDockerCommand {
                operation,
                args,
                command_timeout: DOCKER_ARCHIVE_TIMEOUT,
            })
            .await
            .map(|_| ())
            .map_err(|_| match request.operation {
                DatabaseArchiveOperation::Create => ManagedDatabaseError::BackupFailed,
                DatabaseArchiveOperation::Verify => ManagedDatabaseError::RestoreArchiveInvalid,
                DatabaseArchiveOperation::Restore => ManagedDatabaseError::RestoreFailed,
            })
    }

    async fn copy_container_archive(
        &self,
        request: ContainerArchiveCopy<'_>,
    ) -> Result<(), ManagedDatabaseError> {
        let container_path = OsString::from(format!(
            "{}:{}",
            request.container.name, request.container.path
        ));
        let host_path = request.host_path.as_os_str().to_os_string();
        let (operation, args, failure) = match request.direction {
            ContainerCopyDirection::FromContainer => (
                "copy managed database archive",
                vec![
                    OsString::from("container"),
                    OsString::from("cp"),
                    container_path,
                    host_path,
                ],
                ManagedDatabaseError::BackupFailed,
            ),
            ContainerCopyDirection::ToContainer => (
                "copy managed restore archive",
                vec![
                    OsString::from("container"),
                    OsString::from("cp"),
                    host_path,
                    container_path,
                ],
                ManagedDatabaseError::RestoreArchiveInvalid,
            ),
        };
        self.runner
            .checked_os_with_timeout(CheckedDockerCommand {
                operation,
                args,
                command_timeout: DOCKER_ARCHIVE_TIMEOUT,
            })
            .await
            .map(|_| ())
            .map_err(|_| failure)
    }

    pub(super) async fn remove_container_archive(
        &self,
        name: &str,
        container_path: &str,
    ) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked(
                "remove managed temporary archive",
                ["exec", name, "rm", "--force", "--", container_path],
            )
            .await
            .map(|_| ())
            .map_err(|_| ManagedDatabaseError::ArchiveCleanupFailed)
    }
}

impl DockerContainerCli<'_> {
    pub(super) async fn rename_container(
        &self,
        current: &str,
        replacement: &str,
    ) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked(
                "rename managed container",
                ["container", "rename", current, replacement],
            )
            .await
            .map(|_| ())
    }

    pub(super) async fn remove_container(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked_os("remove managed container", remove_container_arguments(name))
            .await
            .map(|_| ())
    }
}

impl DockerVolumeCli<'_> {
    pub(super) async fn remove_volume(&self, name: &str) -> Result<(), ManagedDatabaseError> {
        self.runner
            .checked("remove managed volume", ["volume", "rm", name])
            .await
            .map(|_| ())
    }
}

async fn stop_container_with_grace(
    runner: &DockerCommandRunner,
    name: &str,
    policy: StopPolicy,
) -> Result<(), ManagedDatabaseError> {
    let output = runner
        .execute_os_with_timeout(
            stop_container_arguments(name, policy),
            policy.command_timeout,
        )
        .await?;
    if !output.status.success() {
        return Err(ManagedDatabaseError::DockerOperation {
            operation: "stop managed container",
        });
    }
    Ok(())
}

fn stop_container_arguments(name: &str, policy: StopPolicy) -> Vec<OsString> {
    ["container", "stop", "--time", policy.grace_seconds, name]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn remove_container_arguments(name: &str) -> Vec<OsString> {
    ["container", "rm", "--force", "--volumes", name]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn password_copy_arguments(name: &str, password_file: &Path) -> Vec<OsString> {
    vec![
        OsString::from("container"),
        OsString::from("cp"),
        password_file.as_os_str().to_os_string(),
        OsString::from(format!("{name}:{CONTAINER_PASSWORD_PATH}")),
    ]
}

#[derive(Clone, Copy)]
enum DatabaseArchiveMode {
    Backup,
    Restore,
}

fn database_archive_arguments(
    name: &str,
    container_path: &str,
    mode: DatabaseArchiveMode,
) -> Vec<OsString> {
    let mut arguments = [
        "exec",
        name,
        "timeout",
        "--signal=TERM",
        "--kill-after",
        CONTAINER_ARCHIVE_KILL_AFTER,
        CONTAINER_ARCHIVE_TIMEOUT,
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    match mode {
        DatabaseArchiveMode::Backup => arguments.extend(
            [
                "pg_dump",
                "--username",
                DATABASE_USER,
                "--dbname",
                DATABASE_NAME,
                "--format=custom",
                "--no-owner",
                "--no-privileges",
                "--file",
                container_path,
            ]
            .into_iter()
            .map(OsString::from),
        ),
        DatabaseArchiveMode::Restore => arguments.extend(
            [
                "pg_restore",
                "--username",
                DATABASE_USER,
                "--dbname",
                MAINTENANCE_DATABASE_NAME,
                "--create",
                "--exit-on-error",
                "--no-owner",
                "--no-privileges",
                container_path,
            ]
            .into_iter()
            .map(OsString::from),
        ),
    }
    arguments
}

fn database_backup_arguments(name: &str, container_path: &str) -> Vec<OsString> {
    database_archive_arguments(name, container_path, DatabaseArchiveMode::Backup)
}

fn database_archive_list_arguments(name: &str, container_path: &str) -> Vec<OsString> {
    [
        "exec",
        name,
        "timeout",
        "--signal=TERM",
        "--kill-after",
        CONTAINER_ARCHIVE_KILL_AFTER,
        CONTAINER_ARCHIVE_TIMEOUT,
        "pg_restore",
        "--list",
        container_path,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn database_restore_arguments(name: &str, container_path: &str) -> Vec<OsString> {
    database_archive_arguments(name, container_path, DatabaseArchiveMode::Restore)
}

fn database_reset_arguments(name: &str) -> Vec<OsString> {
    [
        "exec",
        name,
        "timeout",
        "--signal=TERM",
        "--kill-after",
        CONTAINER_ARCHIVE_KILL_AFTER,
        CONTAINER_ARCHIVE_TIMEOUT,
        "dropdb",
        "--username",
        DATABASE_USER,
        "--force",
        "--if-exists",
        DATABASE_NAME,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

pub(super) async fn initialize_extensions(
    docker: &DockerCli,
    name: &str,
) -> Result<(), ManagedDatabaseError> {
    docker
        .runner
        .checked_os(
            "initialize PostgreSQL extensions",
            initialize_extension_arguments(name),
        )
        .await
        .map(|_| ())
}

fn initialize_extension_arguments(name: &str) -> Vec<OsString> {
    let pg_search_upgrade =
        format!("ALTER EXTENSION pg_search UPDATE TO '{SUPPORTED_PG_SEARCH_VERSION}';");
    let pgvector_upgrade =
        format!("ALTER EXTENSION vector UPDATE TO '{MANAGED_PGVECTOR_VERSION}';");
    vec![
        OsString::from("exec"),
        OsString::from(name),
        OsString::from("psql"),
        OsString::from("--username"),
        OsString::from(DATABASE_USER),
        OsString::from("--dbname"),
        OsString::from(DATABASE_NAME),
        OsString::from("--set"),
        OsString::from("ON_ERROR_STOP=1"),
        OsString::from("--single-transaction"),
        OsString::from("--command"),
        OsString::from("CREATE EXTENSION IF NOT EXISTS vector;"),
        OsString::from("--command"),
        OsString::from(pgvector_upgrade),
        OsString::from("--command"),
        OsString::from("CREATE EXTENSION IF NOT EXISTS pg_search;"),
        OsString::from("--command"),
        OsString::from(pg_search_upgrade),
        OsString::from("--command"),
        OsString::from("CREATE EXTENSION IF NOT EXISTS pgstattuple;"),
    ]
}

async fn inspect_volume_labels(
    docker: &DockerVolumeCli<'_>,
    identity: &ManagedResourceIdentity,
) -> Result<Option<String>, ManagedDatabaseError> {
    let existing = docker
        .runner
        .execute([
            "volume",
            "inspect",
            "--format",
            VOLUME_INSPECT_TEMPLATE,
            &identity.volume_name,
        ])
        .await?;
    if existing.status.success() {
        return Ok(Some(trim_line_end(&existing.stdout).to_owned()));
    }
    if existing.stderr.to_ascii_lowercase().contains("no such") {
        return Ok(None);
    }
    Err(ManagedDatabaseError::DockerOperation {
        operation: "inspect managed volume",
    })
}

pub(super) async fn verify_volume(
    docker: &DockerVolumeCli<'_>,
    identity: &ManagedResourceIdentity,
) -> Result<(), ManagedDatabaseError> {
    let labels = inspect_volume_labels(docker, identity)
        .await?
        .ok_or(ManagedDatabaseError::ManagedVolumeMissing)?;
    validate_volume_ownership(&labels, identity)
}

impl DockerCommandRunner {
    fn new() -> Self {
        Self {
            command: OsString::from("docker"),
            endpoint: OnceLock::new(),
        }
    }

    async fn pin_local_endpoint(&self) -> Result<(), ManagedDatabaseError> {
        if self.endpoint.get().is_some() {
            return Ok(());
        }
        let endpoint = self
            .execute_os_unpinned(vec![
                OsString::from("context"),
                OsString::from("inspect"),
                OsString::from("--format"),
                OsString::from("{{.Endpoints.docker.Host}}"),
            ])
            .await?;
        if !endpoint.status.success() {
            return Err(ManagedDatabaseError::DockerOperation {
                operation: "inspect Docker context",
            });
        }
        let endpoint = endpoint.stdout.trim();
        if !is_local_docker_endpoint(endpoint) {
            return Err(ManagedDatabaseError::NonLocalDockerEndpoint);
        }
        let _ = self.endpoint.set(endpoint.to_owned());
        Ok(())
    }

    async fn checked<'a>(
        &self,
        operation: &'static str,
        args: impl IntoIterator<Item = &'a str>,
    ) -> Result<String, ManagedDatabaseError> {
        let args = args.into_iter().map(OsString::from).collect();
        self.checked_os(operation, args).await
    }

    async fn checked_os(
        &self,
        operation: &'static str,
        args: Vec<OsString>,
    ) -> Result<String, ManagedDatabaseError> {
        let output = self.execute_os(args).await?;
        if !output.status.success() {
            return Err(ManagedDatabaseError::DockerOperation { operation });
        }
        Ok(output.stdout)
    }

    async fn checked_os_with_timeout(
        &self,
        command: CheckedDockerCommand,
    ) -> Result<String, ManagedDatabaseError> {
        let output = self
            .execute_os_with_timeout(command.args, command.command_timeout)
            .await?;
        if !output.status.success() {
            return Err(ManagedDatabaseError::DockerOperation {
                operation: command.operation,
            });
        }
        Ok(output.stdout)
    }

    async fn execute<'a>(
        &self,
        args: impl IntoIterator<Item = &'a str>,
    ) -> Result<CommandOutput, ManagedDatabaseError> {
        self.execute_os(args.into_iter().map(OsString::from).collect())
            .await
    }

    async fn execute_os(&self, args: Vec<OsString>) -> Result<CommandOutput, ManagedDatabaseError> {
        self.execute_os_with_timeout(args, DOCKER_COMMAND_TIMEOUT)
            .await
    }

    async fn execute_os_with_timeout(
        &self,
        args: Vec<OsString>,
        command_timeout: Duration,
    ) -> Result<CommandOutput, ManagedDatabaseError> {
        let mut command = Command::new(&self.command);
        let endpoint = self
            .endpoint
            .get()
            .ok_or(ManagedDatabaseError::DockerEndpointNotPinned)?;
        command
            .args(pinned_command_arguments(endpoint, args))
            .kill_on_drop(true);
        run_command(command, command_timeout).await
    }

    async fn execute_os_unpinned(
        &self,
        args: Vec<OsString>,
    ) -> Result<CommandOutput, ManagedDatabaseError> {
        let mut command = Command::new(&self.command);
        command.args(args).kill_on_drop(true);
        run_command(command, DOCKER_COMMAND_TIMEOUT).await
    }
}

fn pinned_command_arguments(endpoint: &str, args: Vec<OsString>) -> Vec<OsString> {
    let mut pinned = Vec::with_capacity(args.len() + 2);
    pinned.push(OsString::from("--host"));
    pinned.push(OsString::from(endpoint));
    pinned.extend(args);
    pinned
}

async fn run_command(
    mut command: Command,
    command_timeout: Duration,
) -> Result<CommandOutput, ManagedDatabaseError> {
    let output = timeout(command_timeout, command.output())
        .await
        .map_err(|_| ManagedDatabaseError::DockerTimeout)?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ManagedDatabaseError::DockerUnavailable
            } else {
                ManagedDatabaseError::DockerOperation {
                    operation: "launch Docker command",
                }
            }
        })?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy_owned(output.stdout),
        stderr: String::from_utf8_lossy_owned(output.stderr),
    })
}

fn is_local_docker_endpoint(endpoint: &str) -> bool {
    let Ok(endpoint) = url::Url::parse(endpoint) else {
        return false;
    };
    match endpoint.scheme() {
        "unix" | "npipe" | "fd" => true,
        "tcp" | "http" | "https" => endpoint.host().is_some_and(|host| match host {
            url::Host::Domain(domain) => {
                domain.eq_ignore_ascii_case("localhost")
                    || domain
                        .trim_matches(['[', ']'])
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        }),
        _ => false,
    }
}

fn create_container_arguments(spec: &ContainerCreateSpec<'_>) -> Vec<OsString> {
    [
        "container".to_owned(),
        "create".to_owned(),
        "--name".to_owned(),
        spec.identity.container_name.clone(),
        "--label".to_owned(),
        "io.cartograph.managed=true".to_owned(),
        "--label".to_owned(),
        format!("io.cartograph.project={}", spec.identity.project_hash),
        "--env".to_owned(),
        format!("POSTGRES_USER={DATABASE_USER}"),
        "--env".to_owned(),
        format!("POSTGRES_DB={DATABASE_NAME}"),
        "--env".to_owned(),
        format!("POSTGRES_PASSWORD_FILE={CONTAINER_PASSWORD_PATH}"),
        "--publish".to_owned(),
        format!("127.0.0.1:{}:5432", spec.port),
        "--shm-size".to_owned(),
        MANAGED_DATABASE_SHARED_MEMORY_BYTES.to_string(),
        "--memory".to_owned(),
        MANAGED_DATABASE_MEMORY_LIMIT_BYTES.to_string(),
        "--memory-reservation".to_owned(),
        MANAGED_DATABASE_MEMORY_RESERVATION_BYTES.to_string(),
        "--cpus".to_owned(),
        (MANAGED_DATABASE_NANO_CPUS / 1_000_000_000).to_string(),
        "--pids-limit".to_owned(),
        MANAGED_DATABASE_PIDS_LIMIT.to_string(),
        "--mount".to_owned(),
        format!(
            "type=volume,source={},target={DATABASE_DATA_PATH}/",
            spec.identity.volume_name,
        ),
        "--health-cmd".to_owned(),
        "pg_isready -h 127.0.0.1 -U cartograph -d cartograph".to_owned(),
        "--health-interval".to_owned(),
        "2s".to_owned(),
        "--health-timeout".to_owned(),
        "3s".to_owned(),
        "--health-start-period".to_owned(),
        "5s".to_owned(),
        "--health-retries".to_owned(),
        "30".to_owned(),
        "--restart".to_owned(),
        "unless-stopped".to_owned(),
        spec.image.to_owned(),
    ]
    .into_iter()
    .chain(spec.command.iter().cloned())
    .map(OsString::from)
    .collect()
}

fn trim_line_end(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn join_output_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{}\n{}", trim_line_end(stdout), stderr),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (true, true) => String::new(),
    }
}

fn parse_container_inspection(value: &str) -> Result<ContainerInspection, ManagedDatabaseError> {
    let fields: Vec<_> = value.split('\t').collect();
    let [
        managed,
        project_hash,
        process_state,
        health_state,
        image,
        host_ip,
        host_port,
        shared_memory_bytes,
        memory_limit_bytes,
        memory_reservation_bytes,
        nano_cpus,
        pids_limit,
        mounts,
        command,
    ] = fields.as_slice()
    else {
        return Err(ManagedDatabaseError::DockerResponse);
    };
    // Docker prints `null` for a container created without a command; that
    // simply is not the current command.
    let command: Vec<String> = serde_json::from_str::<Option<Vec<String>>>(command)
        .map_err(|_| ManagedDatabaseError::DockerResponse)?
        .unwrap_or_default();
    let mounts: Vec<ContainerMountInspection> =
        serde_json::from_str(mounts).map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let shared_memory_bytes = shared_memory_bytes
        .parse::<u64>()
        .map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let memory_limit_bytes = memory_limit_bytes
        .parse::<u64>()
        .map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let memory_reservation_bytes = memory_reservation_bytes
        .parse::<u64>()
        .map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let nano_cpus = nano_cpus
        .parse::<u64>()
        .map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let pids_limit = pids_limit
        .parse::<i64>()
        .map_err(|_| ManagedDatabaseError::DockerResponse)?;
    let mut data_mounts = mounts
        .into_iter()
        .filter(|mount| mount.destination.trim_end_matches('/') == DATABASE_DATA_PATH);
    let data_mount = data_mounts.next();
    if data_mounts.next().is_some() {
        return Err(ManagedDatabaseError::DockerResponse);
    }
    Ok(ContainerInspection {
        managed: *managed == "true",
        project_hash: (*project_hash).to_owned(),
        process_state: (*process_state).to_owned(),
        health_state: (*health_state).to_owned(),
        image: (*image).to_owned(),
        host_ip: (*host_ip).to_owned(),
        host_port: (*host_port).to_owned(),
        shared_memory_bytes,
        memory_limit_bytes,
        memory_reservation_bytes,
        nano_cpus,
        pids_limit,
        data_mount,
        command,
    })
}

pub(super) fn has_expected_data_mount(
    inspection: &ContainerInspection,
    identity: &ManagedResourceIdentity,
) -> bool {
    inspection.data_mount.as_ref().is_some_and(|mount| {
        mount.mount_type == "volume"
            && mount.name.as_deref() == Some(identity.volume_name.as_str())
            && mount.destination.trim_end_matches('/') == DATABASE_DATA_PATH
            && mount.read_write
    })
}

fn validate_volume_ownership(
    value: &str,
    identity: &ManagedResourceIdentity,
) -> Result<(), ManagedDatabaseError> {
    let fields: Vec<_> = value.split('\t').collect();
    let [managed, project_hash] = fields.as_slice() else {
        return Err(ManagedDatabaseError::DockerResponse);
    };
    if *managed != "true" || *project_hash != identity.project_hash {
        return Err(ManagedDatabaseError::ForeignVolume);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    const LOCAL_DOCKER_ENDPOINTS: [&str; 6] = [
        "unix:///var/run/docker.sock",
        "npipe:////./pipe/docker_engine",
        "fd://",
        "tcp://127.0.0.1:2375",
        "tcp://[::1]:2375",
        "https://localhost:2376",
    ];
    const REMOTE_DOCKER_ENDPOINTS: [&str; 4] = [
        "ssh://builder@example.invalid",
        "tcp://192.0.2.10:2375",
        "https://user:secret@example.invalid:2376",
        "not-an-endpoint",
    ];

    fn identity() -> ManagedResourceIdentity {
        ManagedResourceIdentity {
            project_hash: "0123456789abcdef".to_owned(),
            container_name: "cartograph-v2-0123456789abcdef".to_owned(),
            volume_name: "cartograph-v2-0123456789abcdef-data".to_owned(),
        }
    }

    #[test]
    fn create_arguments_publish_only_loopback_and_keep_secret_out_of_process_args() {
        let identity = identity();
        let command = postgres_command();
        let args = create_container_arguments(&ContainerCreateSpec {
            identity: &identity,
            port: 55_432,
            image: "paradedb/example@sha256:digest",
            command: &command,
        });
        let rendered: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();

        assert!(rendered.iter().any(|arg| arg == "127.0.0.1:55432:5432"));
        assert!(rendered.iter().any(|arg| arg == "--shm-size"));
        assert!(
            rendered
                .iter()
                .any(|arg| arg == &MANAGED_DATABASE_SHARED_MEMORY_BYTES.to_string())
        );
        assert!(
            rendered
                .iter()
                .any(|arg| arg == "POSTGRES_PASSWORD_FILE=/tmp/cartograph-postgres-password")
        );
        assert!(!rendered.iter().any(|arg| arg == "--env-file"));
        assert!(
            rendered
                .iter()
                .any(|arg| arg == "pg_isready -h 127.0.0.1 -U cartograph -d cartograph")
        );
        assert!(rendered.iter().any(|arg| arg == "--health-start-period"));
        assert!(
            !rendered
                .iter()
                .any(|arg| arg.starts_with("POSTGRES_PASSWORD="))
        );
        assert!(!rendered.iter().any(|arg| arg.contains("password=")));
        assert!(
            rendered
                .iter()
                .any(|arg| arg == "io.cartograph.managed=true")
        );
        for expected in [
            "--memory",
            "--memory-reservation",
            "--cpus",
            "--pids-limit",
            "shared_buffers=256MB",
            "effective_cache_size=1GB",
            "maintenance_work_mem=128MB",
            "autovacuum_work_mem=64MB",
            "work_mem=4MB",
            "checkpoint_timeout=15min",
            "max_wal_size=2GB",
            "min_wal_size=256MB",
            "wal_compression=lz4",
            "max_connections=96",
            "max_worker_processes=16",
            "max_parallel_workers=4",
            "max_parallel_maintenance_workers=2",
        ] {
            assert!(
                rendered.iter().any(|arg| arg == expected),
                "managed create arguments omitted {expected}"
            );
        }
        for (flag, value) in [
            ("--memory", MANAGED_DATABASE_MEMORY_LIMIT_BYTES.to_string()),
            (
                "--memory-reservation",
                MANAGED_DATABASE_MEMORY_RESERVATION_BYTES.to_string(),
            ),
            (
                "--cpus",
                (MANAGED_DATABASE_NANO_CPUS / 1_000_000_000).to_string(),
            ),
            ("--pids-limit", MANAGED_DATABASE_PIDS_LIMIT.to_string()),
        ] {
            assert!(
                rendered
                    .array_windows()
                    .any(|[argument, bound]| *argument == flag && *bound == value),
                "managed create arguments did not bind {flag} to its supported value"
            );
        }
    }

    #[test]
    fn extension_initialization_installs_pgvector_before_pg_search_and_heap_measurement() {
        let arguments = initialize_extension_arguments("cartograph-v2-test");
        let commands: Vec<_> = arguments
            .array_windows()
            .filter(|[flag, _]| *flag == "--command")
            .map(|[_, command]| command.to_string_lossy())
            .collect();

        assert_eq!(
            commands,
            [
                "CREATE EXTENSION IF NOT EXISTS vector;",
                "ALTER EXTENSION vector UPDATE TO '0.8.4';",
                "CREATE EXTENSION IF NOT EXISTS pg_search;",
                "ALTER EXTENSION pg_search UPDATE TO '0.25.11';",
                "CREATE EXTENSION IF NOT EXISTS pgstattuple;",
            ]
        );
    }

    #[test]
    fn inspection_parser_preserves_owner_state_health_and_image() {
        let parsed = parse_container_inspection(
            "true\t0123456789abcdef\trunning\thealthy\tparadedb/example@sha256:digest\t127.0.0.1\t55432\t268435456\t2147483648\t1073741824\t4000000000\t256\t[{\"Type\":\"volume\",\"Name\":\"cartograph-v2-0123456789abcdef-data\",\"Destination\":\"/var/lib/postgresql\",\"RW\":true}]\t[\"postgres\",\"-c\",\"max_wal_size=4GB\"]",
        );
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => panic!("valid inspect response failed: {error}"),
        };

        assert!(parsed.managed);
        assert_eq!(parsed.project_hash, "0123456789abcdef");
        assert_eq!(parsed.process_state, "running");
        assert_eq!(parsed.health_state, "healthy");
        assert_eq!(parsed.image, "paradedb/example@sha256:digest");
        assert_eq!(parsed.host_ip, "127.0.0.1");
        assert_eq!(parsed.host_port, "55432");
        assert_eq!(parsed.shared_memory_bytes, 268_435_456);
        assert_eq!(parsed.memory_limit_bytes, 2_147_483_648);
        assert_eq!(parsed.memory_reservation_bytes, 1_073_741_824);
        assert_eq!(parsed.nano_cpus, 4_000_000_000);
        assert_eq!(parsed.pids_limit, 256);
        assert!(has_expected_data_mount(&parsed, &identity()));
        assert_eq!(parsed.command, ["postgres", "-c", "max_wal_size=4GB"]);
        assert!(!has_current_postgres_settings(&parsed));
    }

    #[test]
    fn only_the_current_command_counts_as_current_settings() {
        let current = postgres_command();
        // The WAL settings changed; everything else is carried over unchanged.
        let changed = current
            .iter()
            .filter(|argument| !LEGACY_POSTGRES_COMMAND.contains(&argument.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            changed,
            [
                "max_wal_size=2GB",
                "min_wal_size=256MB",
                "wal_compression=lz4"
            ]
        );
        let with = |command: Vec<String>| {
            ContainerInspection {
            command,
            ..parse_container_inspection(
                "true\t0123456789abcdef\trunning\thealthy\timage\t127.0.0.1\t55432\t1\t1\t1\t1\t1\t[]\t[]",
            )
            .unwrap_or_else(|error| panic!("inspection fixture failed: {error}"))
        }
        };
        assert!(has_current_postgres_settings(&with(current)));
        assert!(!has_current_postgres_settings(&with(
            LEGACY_POSTGRES_COMMAND.map(str::to_owned).to_vec()
        )));
    }

    #[test]
    fn volume_ownership_rejects_foreign_resources() {
        assert!(validate_volume_ownership("true\t0123456789abcdef", &identity()).is_ok());
        assert_matches!(
            validate_volume_ownership("<no value>\t<no value>", &identity()),
            Err(ManagedDatabaseError::ForeignVolume)
        );
    }

    #[test]
    fn malformed_inspection_is_rejected_instead_of_assumed_owned() {
        assert_matches!(
            parse_container_inspection("true\tonly-two-fields").err(),
            Some(ManagedDatabaseError::DockerResponse)
        );
        let without_command = parse_container_inspection(
            "true\t0123456789abcdef\trunning\thealthy\timage\t127.0.0.1\t55432\t1\t1\t1\t1\t1\t[]\tnull",
        )
        .unwrap_or_else(|error| panic!("a null command must still parse: {error}"));
        assert_eq!(without_command.command, [] as [String; 0]);
        assert!(!has_current_postgres_settings(&without_command));
    }

    #[test]
    fn docker_endpoint_policy_accepts_only_local_transports() {
        for local in LOCAL_DOCKER_ENDPOINTS {
            assert!(
                is_local_docker_endpoint(local),
                "rejected local endpoint: {local}"
            );
        }
        for remote in REMOTE_DOCKER_ENDPOINTS {
            assert!(
                !is_local_docker_endpoint(remote),
                "accepted remote endpoint"
            );
        }
        assert!(
            !ManagedDatabaseError::NonLocalDockerEndpoint
                .to_string()
                .contains("secret")
        );
    }

    #[test]
    fn effectful_commands_are_bound_to_the_validated_endpoint() {
        let arguments = pinned_command_arguments(
            "unix:///private/docker.sock",
            vec![OsString::from("container"), OsString::from("inspect")],
        );
        let rendered: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();

        assert_eq!(
            rendered,
            [
                "--host",
                "unix:///private/docker.sock",
                "container",
                "inspect"
            ]
        );
    }

    #[test]
    fn normal_stop_budget_has_cushion_beyond_postgres_grace() {
        let arguments = stop_container_arguments("cartograph-v2-test", NORMAL_STOP_POLICY);
        let rendered: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();

        assert_eq!(
            rendered,
            ["container", "stop", "--time", "30", "cartograph-v2-test"]
        );
        assert!(NORMAL_STOP_POLICY.command_timeout > Duration::from_secs(30));
    }

    #[test]
    fn container_removal_collects_only_attached_anonymous_volumes() {
        let arguments = remove_container_arguments("cartograph-v2-test");
        let rendered: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();

        assert_eq!(
            rendered,
            [
                "container",
                "rm",
                "--force",
                "--volumes",
                "cartograph-v2-test"
            ]
        );
    }

    #[test]
    fn archive_commands_use_fixed_database_identity_without_secret_arguments() {
        let backup = database_backup_arguments("cartograph-v2-test", "/tmp/managed.dump");
        let restore = database_restore_arguments("cartograph-v2-test", "/tmp/managed.dump");
        let reset = database_reset_arguments("cartograph-v2-test");
        let rendered_backup: Vec<_> = backup
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();
        let rendered_restore: Vec<_> = restore
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();
        let rendered_reset: Vec<_> = reset
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();

        assert!(rendered_backup.iter().any(|argument| argument == "pg_dump"));
        assert!(rendered_backup.iter().any(|argument| argument == "timeout"));
        assert!(
            rendered_backup
                .iter()
                .any(|argument| argument == CONTAINER_ARCHIVE_TIMEOUT)
        );
        assert!(
            rendered_backup
                .iter()
                .any(|argument| argument == "--format=custom")
        );
        assert!(
            rendered_restore
                .iter()
                .any(|argument| argument == "pg_restore")
        );
        assert!(
            !rendered_restore
                .iter()
                .any(|argument| argument == "--single-transaction")
        );
        assert!(
            rendered_restore
                .iter()
                .any(|argument| argument == "--exit-on-error")
        );
        assert!(
            rendered_restore
                .iter()
                .any(|argument| argument == "--create")
        );
        assert!(
            rendered_restore
                .array_windows()
                .any(|pair| *pair == ["--dbname", MAINTENANCE_DATABASE_NAME])
        );
        assert!(rendered_reset.iter().any(|argument| argument == "dropdb"));
        assert!(rendered_reset.iter().any(|argument| argument == "--force"));
        assert!(
            !rendered_backup
                .iter()
                .any(|argument| argument.contains("password"))
        );
        assert!(
            !rendered_restore
                .iter()
                .any(|argument| argument.contains("password"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn password_copy_preserves_non_utf8_project_paths() {
        use std::{os::unix::ffi::OsStringExt, path::PathBuf};

        let password_file = PathBuf::from(OsString::from_vec(
            b"/tmp/cartograph-non-utf8-\xff/postgres.password".to_vec(),
        ));
        let arguments = password_copy_arguments("cartograph-v2-test", &password_file);

        assert!(
            arguments
                .iter()
                .any(|argument| argument == password_file.as_os_str())
        );
    }

    #[test]
    fn log_streams_include_postgres_stderr_without_dropping_stdout() {
        assert_eq!(join_output_streams("", "postgres log\n"), "postgres log\n");
        assert_eq!(join_output_streams("app log\n", "",), "app log\n");
        assert_eq!(
            join_output_streams("app log\n", "postgres log\n"),
            "app log\npostgres log\n"
        );
    }
}
