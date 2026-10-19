mod native;

use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId, ProjectId};
use serde::Serialize;

const MAX_ROOT_IDENTITY_BYTES: usize = 4_096;

/// Exact row counts for the atomically published generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GenerationCounts {
    /// Source files in the generation.
    pub files: i64,
    /// Extracted symbols in the generation.
    pub symbols: i64,
    /// Resolved graph edges in the generation.
    pub edges: i64,
    /// Resolved and unresolved source references in the generation.
    pub references: i64,
    /// Privacy-safe static numerical evidence sites in the generation.
    pub numerical_sites: i64,
    /// File and symbol documents available to native search.
    pub documents: i64,
}

/// Project-wide generation retention counts and a source/BM25 lower bound.
/// Shared fact-table heaps, B-trees, embeddings, and reusable dead space are
/// intentionally reported by the fuller storage-usage surface instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GenerationStorageSummary {
    /// Generations staged but not yet prepared.
    pub staging: u64,
    /// Prepared generations awaiting publication or recovery.
    pub ready: u64,
    /// Visible generations; valid project state is zero or one.
    pub current: u64,
    /// Historical generations replaced by a newer publication.
    pub superseded: u64,
    /// Terminal failed generations awaiting bounded cleanup.
    pub failed: u64,
    /// Retired generations with committed partial cleanup awaiting completion.
    pub retiring: u64,
    /// Age since reservation of the oldest failed/superseded/retiring generation; zero when none.
    pub oldest_terminal_age_seconds: u64,
    /// Sum of source byte sizes represented by all retained generations.
    pub source_bytes: u64,
    /// Exact physical bytes occupied by generation-scoped search tables and indexes.
    pub search_relation_bytes: u64,
    /// Conservative lower-bound estimate: source bytes plus search-relation bytes.
    pub estimated_retained_bytes: u64,
}

impl GenerationStorageSummary {
    /// Total durable generation rows retained for the project.
    #[must_use]
    pub const fn generations(self) -> u64 {
        self.staging + self.ready + self.current + self.superseded + self.failed + self.retiring
    }
}

/// Durable metadata for one project's visible generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectCurrentGeneration {
    /// Immutable generation identity.
    pub generation_id: GenerationId,
    /// Monotonic project-local publication sequence.
    pub sequence: i64,
    /// Complete source-manifest digest captured before indexing.
    pub source_revision: String,
    /// Deterministic logical fact digest.
    pub content_digest: ContentDigest,
    /// Version used to interpret the logical digest.
    pub digest_version: GenerationDigestVersion,
    /// Exact persisted relation counts.
    pub counts: GenerationCounts,
    /// Privacy-preserving description of the generation's source admission policy.
    pub source_admission: GenerationSourceAdmission,
}

/// Run-scoped source admission metadata. Serialization and debug output reveal
/// only the count; the exact globs remain available solely to trusted runtime
/// freshness and reconciliation code.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GenerationSourceAdmission {
    #[serde(skip)]
    run_excludes: Vec<String>,
    run_exclude_patterns: u16,
    maximum_file_bytes: Option<usize>,
}

impl std::fmt::Debug for GenerationSourceAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationSourceAdmission")
            .field("run_exclude_patterns", &self.run_exclude_patterns)
            .finish_non_exhaustive()
    }
}

impl GenerationSourceAdmission {
    /// Exact run-scoped excludes for trusted source-policy reconciliation.
    #[must_use]
    pub fn run_excludes(&self) -> &[String] {
        &self.run_excludes
    }

    /// Exact run-scoped source-file byte ceiling for trusted freshness checks.
    #[must_use]
    pub const fn maximum_file_bytes(&self) -> Option<usize> {
        self.maximum_file_bytes
    }

    /// Number of persisted run-scoped exclude patterns.
    #[must_use]
    pub const fn run_exclude_patterns(&self) -> u16 {
        self.run_exclude_patterns
    }
}

/// Read-only project state used by CLI, MCP, and freshness checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectSnapshot {
    /// Stable database identity for the project.
    pub project_id: ProjectId,
    /// The currently visible generation, absent before first publication.
    pub current: Option<ProjectCurrentGeneration>,
    /// Project-wide generation retention and storage pressure evidence.
    pub generation_storage: GenerationStorageSummary,
}

fn validate_root_identity(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > MAX_ROOT_IDENTITY_BYTES || value.contains('\0') {
        return Err(StorageError::InvalidInput {
            field: "root_identity",
        });
    }
    Ok(())
}

use crate::StorageError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_identity_rejects_empty_unbounded_and_nul_values() {
        assert_eq!(
            validate_root_identity(""),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
        assert_eq!(
            validate_root_identity(&"x".repeat(MAX_ROOT_IDENTITY_BYTES + 1)),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
        assert_eq!(
            validate_root_identity("project\0escape"),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
    }
}
