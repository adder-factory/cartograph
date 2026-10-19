//! Validated project configuration and native storage migration markers.

mod numeric;
mod project;
mod storage;

pub use storage::{StorageMigrationGuard, record_rocksdb_migration, storage_migration_guard};

pub use numeric::{BoundedU64Field, optional_bounded_u64};

pub use project::{
    ProjectConfigError, ProjectConfigSnapshot, ProjectGenerationStorage, ProjectSourceSettings,
    load_project_source_settings, read_project_config, read_project_config_snapshot,
    update_project_config, write_project_config_if_unchanged, write_project_max_file_size,
};
