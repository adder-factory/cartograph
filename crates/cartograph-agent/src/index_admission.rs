//! Terminal-backlog admission for index attempts.
//!
//! Repeated automatic failures on a large corpus reserve complete generations
//! faster than bounded cleanup can delete them, so an automatic attempt whose
//! project is over the backlog bound first drains eligible generations once.
//! An unresolved backlog defers automatic indexing even when cleanup cannot
//! make progress. Transient lease contention returns the ordinary busy outcome
//! before a new generation can be reserved.
//! Explicit requests are never deferred.
//!
//! This module owns the decision and the order of observations. The runtime
//! supplies the I/O, counting and draining, through [`TerminalBacklog`].

use crate::{IndexFailureRetention, ProjectError};

/// Failed or partially retired generations an automatic attempt may leave
/// behind before the next automatic attempt must drain them first. Without
/// this admission bound, repeated automatic failures on a large corpus reserve
/// complete generations faster than bounded cleanup can delete them.
const AUTOMATIC_TERMINAL_BACKLOG_LIMIT: u64 = 1;

/// One project's terminal-generation backlog as the admission policy observes it.
pub(crate) trait TerminalBacklog {
    /// Eligible failed, retiring, and excess superseded generations.
    async fn count(&self) -> Result<u64, ProjectError>;

    /// Run one bounded pass, distinguishing lease contention from an attempted drain.
    async fn drain(&self) -> BacklogDrain;
}

/// Whether retention could enter its fenced maintenance operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BacklogDrain {
    /// Another live operation owns the project; defer before reservation.
    ProjectBusy,
    /// Cleanup ran or failed; recount before admitting another generation.
    Attempted,
}

/// Whether an index attempt may reserve its generation now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BacklogAdmission {
    /// Reserve the generation.
    Admit,
    /// The backlog remains over bound after a bounded maintenance attempt.
    Defer,
}

impl BacklogAdmission {
    /// The index outcome this admission implies: a deferral is
    /// [`ProjectError::IndexRetentionBacklog`].
    pub(crate) const fn into_result(self) -> Result<(), ProjectError> {
        match self {
            Self::Admit => Ok(()),
            Self::Defer => Err(ProjectError::IndexRetentionBacklog),
        }
    }
}

/// Decide whether an attempt may reserve a generation.
///
/// `backlog` is `None` for a project with no prior snapshot, which has nothing
/// to drain. The backlog is counted only for automatic attempts, drained only
/// when over bound, and counted again unless another live writer owns the project.
///
/// # Errors
///
/// Returns the backlog's error when a count cannot be read.
pub(crate) async fn admit_index_attempt<B: TerminalBacklog>(
    failure_retention: IndexFailureRetention,
    backlog: Option<&B>,
) -> Result<BacklogAdmission, ProjectError> {
    let (IndexFailureRetention::AutomaticFailures, Some(backlog)) = (failure_retention, backlog)
    else {
        return Ok(BacklogAdmission::Admit);
    };
    if backlog.count().await? <= AUTOMATIC_TERMINAL_BACKLOG_LIMIT {
        return Ok(BacklogAdmission::Admit);
    }
    if backlog.drain().await == BacklogDrain::ProjectBusy {
        return Err(ProjectError::IndexLeaseBusy);
    }
    if backlog.count().await? > AUTOMATIC_TERMINAL_BACKLOG_LIMIT {
        return Ok(BacklogAdmission::Defer);
    }
    Ok(BacklogAdmission::Admit)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;

    const OVER_BOUND: u64 = AUTOMATIC_TERMINAL_BACKLOG_LIMIT + 1;

    /// Answers counts in order and a fixed drain outcome, logging each call.
    struct Scripted {
        counts: RefCell<VecDeque<u64>>,
        drain: BacklogDrain,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Scripted {
        fn new(counts: &[u64], drain: BacklogDrain) -> Self {
            Self {
                counts: RefCell::new(counts.iter().copied().collect()),
                drain,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl TerminalBacklog for Scripted {
        fn count(&self) -> impl Future<Output = Result<u64, ProjectError>> {
            self.calls.borrow_mut().push("count");
            let count = self.counts.borrow_mut().pop_front();
            std::future::ready(count.ok_or(ProjectError::StatusFailed))
        }

        fn drain(&self) -> impl Future<Output = BacklogDrain> {
            self.calls.borrow_mut().push("drain");
            std::future::ready(self.drain)
        }
    }

    #[tokio::test]
    async fn explicit_requests_are_admitted_without_reading_the_backlog() -> Result<(), ProjectError>
    {
        let backlog = Scripted::new(&[OVER_BOUND], BacklogDrain::Attempted);
        let admission = admit_index_attempt(
            IndexFailureRetention::SuccessfulRequestsOnly,
            Some(&backlog),
        )
        .await?;
        assert_eq!(admission, BacklogAdmission::Admit);
        assert_eq!(backlog.calls(), [] as [&str; 0]);
        Ok(())
    }

    #[tokio::test]
    async fn transient_project_contention_defers_before_reserving_a_generation() {
        let backlog = Scripted::new(&[OVER_BOUND], BacklogDrain::ProjectBusy);
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&backlog)).await;
        assert!(matches!(admission, Err(ProjectError::IndexLeaseBusy)));
        assert_eq!(backlog.calls(), ["count", "drain"]);
    }

    #[tokio::test]
    async fn an_attempted_drain_defers_even_without_progress_while_over_bound()
    -> Result<(), ProjectError> {
        let stuck = Scripted::new(&[OVER_BOUND, OVER_BOUND], BacklogDrain::Attempted);
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&stuck)).await?;
        assert_eq!(admission, BacklogAdmission::Defer);
        assert_eq!(stuck.calls(), ["count", "drain", "count"]);

        let cleared = Scripted::new(
            &[OVER_BOUND, AUTOMATIC_TERMINAL_BACKLOG_LIMIT],
            BacklogDrain::Attempted,
        );
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&cleared)).await?;
        assert_eq!(admission, BacklogAdmission::Admit);
        Ok(())
    }
}
