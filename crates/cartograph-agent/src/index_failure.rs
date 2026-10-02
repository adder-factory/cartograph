//! Index failure classification: project contention, primary failures, and
//! the secondary outcome of bounded cleanup.
//!
//! An index attempt can fail and then fail again while terminalizing its own
//! staging generation. The first failure decides the stable code and the retry
//! policy; a later cleanup failure is secondary detail, because the next
//! attempt's bounded staging preflight retries that cleanup. Contention with
//! another project writer is reported as [`ProjectError::IndexLeaseBusy`]
//! before any generation is reserved, never as a cleanup failure.

use std::time::Duration;

use cartograph_db::{CartographDatabase, LeaseError, StorageError};
use cartograph_domain::ProjectId;
use cartograph_indexer::{
    CancellationReason, PipelineFailureReason, PipelineStage, SupervisorError,
};

use crate::ProjectError;

/// One failed index attempt: the authoritative primary failure plus whether
/// bounded cleanup of the attempt's own staging generation failed afterward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFailure {
    error: ProjectError,
    cleanup_failed: bool,
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

    /// Fold in the outcome of the attempt's final bounded cleanup of its own
    /// staging generation, which runs after any supervisor-owned cleanup.
    ///
    /// `Ok(true)` means that cleanup terminalized the row itself, so an earlier
    /// owned-cleanup failure no longer leaves a nonterminal staging generation;
    /// normal retention reclaims its storage. `Ok(false)` means the row was
    /// already terminal or is still protected by a live lease, so an earlier
    /// cleanup failure stands. An error leaves the row for the next bounded
    /// staging preflight.
    #[must_use]
    pub(crate) fn after_staging_cleanup(self, cleanup: &Result<bool, StorageError>) -> Self {
        match cleanup {
            Ok(true) => Self {
                cleanup_failed: false,
                ..self
            },
            Ok(false) => self,
            Err(_) => self.with_failed_cleanup(),
        }
    }
}

impl From<ProjectError> for IndexFailure {
    fn from(error: ProjectError) -> Self {
        Self {
            error,
            cleanup_failed: false,
        }
    }
}

/// Whether bounded staging recovery ran or another project writer is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StagingPreflight {
    /// Abandoned staging generations, if any, were terminalized.
    Recovered,
    /// Another operation holds a live project lease or the project lock.
    ProjectBusy,
}

/// Terminalize abandoned staging generations unless another writer is active.
///
/// A live lease on the project, or a project lock still held past the bounded
/// wait (for example, by another writer's long prepare transaction), means a
/// project operation is in flight. Lease acquisition would reject this attempt
/// anyway, so the preflight reports contention instead of a cleanup failure and
/// leaves every row untouched.
///
/// # Errors
///
/// Returns [`ProjectError::IndexLeaseFailed`] when lease state cannot be read
/// and [`ProjectError::IndexCleanupFailed`] when the bounded cleanup fails for
/// any reason other than a project lock wait that timed out.
pub(crate) async fn recover_abandoned_staging(
    database: &CartographDatabase,
    project_id: &ProjectId,
    statement_timeout: Duration,
) -> Result<StagingPreflight, ProjectError> {
    if database
        .has_live_lease(project_id, statement_timeout)
        .await
        .map_err(|_| ProjectError::IndexLeaseFailed)?
    {
        return Ok(StagingPreflight::ProjectBusy);
    }
    match database
        .fail_abandoned_staging_generations_bounded(project_id, statement_timeout)
        .await
    {
        Ok(_) => Ok(StagingPreflight::Recovered),
        Err(StorageError::StatementTimeout { .. }) => Ok(StagingPreflight::ProjectBusy),
        Err(_) => Err(ProjectError::IndexCleanupFailed),
    }
}

/// Run evidence needed to classify one failed supervisor run.
pub(crate) struct SupervisorFailureContext {
    /// Whether the caller's cancellation token fired.
    pub(crate) cancelled: bool,
    /// The stage active when the supervisor stopped.
    pub(crate) stage: Option<PipelineStage>,
}

/// Classify one failed supervisor run.
///
/// An owned-cleanup failure keeps the failure that ended the run as the
/// primary error and is carried only as [`IndexFailure::cleanup_failed`].
pub(crate) fn supervisor_index_failure(
    error: SupervisorError,
    context: &SupervisorFailureContext,
) -> IndexFailure {
    match error {
        SupervisorError::CleanupFailed { primary, .. } => {
            supervisor_index_failure(*primary, context).with_failed_cleanup()
        }
        _ if context.cancelled => ProjectError::RequestCancelled.into(),
        other => supervisor_project_error(other, context.stage).into(),
    }
}

fn supervisor_project_error(error: SupervisorError, stage: Option<PipelineStage>) -> ProjectError {
    match error {
        SupervisorError::Pipeline { stage } => ProjectError::IndexStageFailed { stage },
        SupervisorError::PipelineWithReason { stage, reason } => {
            ProjectError::IndexStageFailedWithReason { stage, reason }
        }
        SupervisorError::PipelineWithFileFailure { stage, failure } => {
            ProjectError::IndexStageFileFailed { stage, failure }
        }
        SupervisorError::Cancelled { reason, .. } => cancelled_project_error(reason, stage),
        other => supervisor_authority_error(&other),
    }
}

const fn cancelled_project_error(
    reason: CancellationReason,
    stage: Option<PipelineStage>,
) -> ProjectError {
    match reason {
        CancellationReason::ProgressStalled => progress_stalled_project_error(stage),
        // The heartbeat could not prove ownership: this is a lease-stage
        // failure, not an unclassified index failure.
        CancellationReason::LeaseLost | CancellationReason::LeaseHeartbeatFailed => {
            ProjectError::IndexLeaseFailed
        }
        CancellationReason::Requested | CancellationReason::OperationDeadline => {
            ProjectError::IndexFailed
        }
    }
}

fn supervisor_authority_error(error: &SupervisorError) -> ProjectError {
    match error {
        SupervisorError::Lease {
            operation: "acquire",
            source: LeaseError::Busy,
        } => ProjectError::IndexLeaseBusy,
        SupervisorError::Lease { .. } | SupervisorError::OwnershipLost { .. } => {
            ProjectError::IndexLeaseFailed
        }
        SupervisorError::Storage { .. }
        | SupervisorError::AmbiguousOutcome { .. }
        | SupervisorError::GenerationMismatch => ProjectError::IndexPublicationFailed,
        _ => ProjectError::IndexFailed,
    }
}

/// A progress stall is attributed to the stage that stopped making progress.
const fn progress_stalled_project_error(stage: Option<PipelineStage>) -> ProjectError {
    match stage {
        Some(stage) => ProjectError::IndexStageFailedWithReason {
            stage,
            reason: PipelineFailureReason::ProgressStalled,
        },
        None => ProjectError::IndexFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: SupervisorFailureContext = SupervisorFailureContext {
        cancelled: false,
        stage: Some(PipelineStage::Parse),
    };

    fn cleanup_after(primary: SupervisorError) -> SupervisorError {
        SupervisorError::CleanupFailed {
            primary: Box::new(primary),
            cleanup: Box::new(SupervisorError::AmbiguousOutcome {
                operation: "cleanup-generation",
            }),
        }
    }

    #[test]
    fn progress_stalls_preserve_the_observed_stage_and_stable_reason() {
        let error = progress_stalled_project_error(Some(PipelineStage::Resolve));
        assert_eq!(
            error,
            ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Resolve,
                reason: PipelineFailureReason::ProgressStalled,
            }
        );
        assert_eq!(
            error.to_string(),
            "Cartograph index operation failed during resolve/progress_stalled; the previous generation remains visible"
        );
        assert_eq!(
            progress_stalled_project_error(None),
            ProjectError::IndexFailed
        );
    }

    #[test]
    fn owned_cleanup_failure_keeps_the_pipeline_failure_primary() {
        let failure = supervisor_index_failure(
            cleanup_after(SupervisorError::Pipeline {
                stage: PipelineStage::Read,
            }),
            &RUNNING,
        );
        assert_eq!(
            failure.error(),
            &ProjectError::IndexStageFailed {
                stage: PipelineStage::Read
            }
        );
        assert!(failure.cleanup_failed());
    }

    #[test]
    fn owned_cleanup_failure_after_contention_stays_retryable_lease_busy() {
        let failure = supervisor_index_failure(
            cleanup_after(SupervisorError::Lease {
                operation: "acquire",
                source: LeaseError::Busy,
            }),
            &RUNNING,
        );
        assert_eq!(failure.error(), &ProjectError::IndexLeaseBusy);
        assert!(failure.cleanup_failed());
    }

    #[test]
    fn final_staging_cleanup_settles_whether_cleanup_failed() {
        let owned_cleanup_failed =
            IndexFailure::from(ProjectError::IndexLeaseFailed).with_failed_cleanup();
        // The final cleanup terminalized the row the owned cleanup could not.
        let settled = owned_cleanup_failed
            .clone()
            .after_staging_cleanup(&Ok(true));
        assert_eq!(settled.error(), &ProjectError::IndexLeaseFailed);
        assert!(!settled.cleanup_failed());
        // Already terminal or still lease-protected: the earlier report stands.
        assert!(
            owned_cleanup_failed
                .after_staging_cleanup(&Ok(false))
                .cleanup_failed()
        );
        assert!(
            !IndexFailure::from(ProjectError::IndexLeaseFailed)
                .after_staging_cleanup(&Ok(false))
                .cleanup_failed()
        );
        // A heartbeat loss whose own prepare transaction still holds the
        // project lock: the lock wait times out, the lease failure stays primary.
        let timed_out = IndexFailure::from(ProjectError::IndexLeaseFailed).after_staging_cleanup(
            &Err(StorageError::StatementTimeout {
                operation: "fail-unleased-staging-lock",
            }),
        );
        assert_eq!(timed_out.error(), &ProjectError::IndexLeaseFailed);
        assert!(timed_out.cleanup_failed());
    }

    #[test]
    fn heartbeat_authority_loss_is_a_lease_failure_not_an_unclassified_one() {
        for reason in [
            CancellationReason::LeaseHeartbeatFailed,
            CancellationReason::LeaseLost,
        ] {
            let failure = supervisor_index_failure(
                SupervisorError::Cancelled {
                    reason,
                    grace_exceeded: false,
                },
                &RUNNING,
            );
            assert_eq!(failure.error(), &ProjectError::IndexLeaseFailed);
            assert!(!failure.cleanup_failed());
        }
    }

    #[test]
    fn caller_cancellation_wins_over_a_wrapped_primary_but_keeps_cleanup_detail() {
        let failure = supervisor_index_failure(
            cleanup_after(SupervisorError::Cancelled {
                reason: CancellationReason::Requested,
                grace_exceeded: false,
            }),
            &SupervisorFailureContext {
                cancelled: true,
                stage: None,
            },
        );
        assert_eq!(failure.error(), &ProjectError::RequestCancelled);
        assert!(failure.cleanup_failed());
    }
}
