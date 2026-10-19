//! Index failure classification: project contention, primary failures, and
//! the secondary outcomes of bounded cleanup and of a SCIP overlay rollback.
//!
//! An index attempt can fail and then fail again while terminalizing its own
//! staging generation. The first failure decides the stable code and the retry
//! policy; a later cleanup failure is secondary detail, because the next
//! attempt's bounded staging preflight retries that cleanup. Contention with
//! another project writer is reported as [`ProjectError::IndexLeaseBusy`]
//! before any generation is reserved, never as a cleanup failure. A SCIP
//! import whose forced index fails restores the previous overlay; a restore
//! that fails as well is secondary detail of the same kind, never a
//! replacement for the index failure.

/// One failed index attempt: the authoritative primary failure plus whether
/// bounded cleanup of the attempt's own staging generation failed afterward
/// and, for a SCIP import, whether restoring the previous overlay failed too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFailure {
    error: ProjectError,
    cleanup_failed: bool,
    overlay_rollback_failed: bool,
}

impl IndexFailure {
    /// The primary failure, which decides the stable code and retry policy.
    #[must_use]
    pub const fn error(&self) -> &ProjectError {
        &self.error
    }

    /// Whether bounded cleanup of this attempt's staging generation also
    /// failed. The next index attempt's staging preflight retries it.
    #[must_use]
    pub const fn cleanup_failed(&self) -> bool {
        self.cleanup_failed
    }

    /// Whether a SCIP import's forced index failed and restoring the
    /// project's previous overlay failed as well. The requested artifact may
    /// then still be installed, and the next index would use it.
    #[must_use]
    pub const fn overlay_rollback_failed(&self) -> bool {
        self.overlay_rollback_failed
    }

    /// Keep only the primary failure.
    #[must_use]
    pub fn into_error(self) -> ProjectError {
        self.error
    }

    /// Record that a cleanup attempted after this failure did not complete.
    #[must_use]
    pub fn with_failed_cleanup(mut self) -> Self {
        self.cleanup_failed = true;
        self
    }

    /// Record that restoring the previous SCIP overlay after this failure did
    /// not complete.
    #[must_use]
    pub fn with_failed_overlay_rollback(mut self) -> Self {
        self.overlay_rollback_failed = true;
        self
    }

    /// Report this failure as the requested cancellation whose cleanup the
    /// caller settles from its own evidence, such as PostgreSQL state.
    ///
    /// The primary becomes [`ProjectError::RequestCancelled`] and the cleanup
    /// outcome is cleared for that evidence to decide. A failed overlay
    /// rollback stays: no database evidence covers the project's files.
    #[must_use]
    pub fn into_requested_cancellation(self) -> Self {
        Self {
            error: ProjectError::RequestCancelled,
            cleanup_failed: false,
            ..self
        }
    }
}

impl From<ProjectError> for IndexFailure {
    fn from(error: ProjectError) -> Self {
        Self {
            error,
            cleanup_failed: false,
            overlay_rollback_failed: false,
        }
    }
}

use crate::ProjectError;
