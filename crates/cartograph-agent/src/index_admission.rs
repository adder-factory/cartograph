//! Terminal-backlog admission for index attempts.
//!
//! Repeated automatic failures on a large corpus reserve complete generations
//! faster than bounded cleanup can delete them, so an automatic attempt whose
//! project is over the backlog bound first drains terminal generations once.
//! The attempt is deferred only while that drain is still making progress: a
//! backlog that cannot clear without operator action (a held project, a
//! search-relation budget, a catalog mismatch) never freezes automatic
//! indexing, and its retention outcome remains visible in storage usage.
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
    /// Failed or partially retired generations still awaiting cleanup.
    async fn count(&self) -> Result<u64, ProjectError>;

    /// Run one bounded retention pass and report whether it committed any
    /// generation cleanup.
    async fn drain(&self) -> bool;
}

/// Whether an index attempt may reserve its generation now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BacklogAdmission {
    /// Reserve the generation.
    Admit,
    /// Retention is still making progress but the backlog remains over bound.
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
/// when over bound, and counted again only after a drain that made progress.
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
    // A drain that commits nothing needs operator action; deferring on it
    // would freeze automatic indexing.
    if !backlog.drain().await {
        return Ok(BacklogAdmission::Admit);
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
        drain_progress: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Scripted {
        fn new(counts: &[u64], drain_progress: bool) -> Self {
            Self {
                counts: RefCell::new(counts.iter().copied().collect()),
                drain_progress,
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

        fn drain(&self) -> impl Future<Output = bool> {
            self.calls.borrow_mut().push("drain");
            std::future::ready(self.drain_progress)
        }
    }

    #[tokio::test]
    async fn explicit_requests_are_admitted_without_reading_the_backlog() -> Result<(), ProjectError>
    {
        let backlog = Scripted::new(&[OVER_BOUND], true);
        let admission = admit_index_attempt(
            IndexFailureRetention::SuccessfulRequestsOnly,
            Some(&backlog),
        )
        .await?;
        assert_eq!(admission, BacklogAdmission::Admit);
        assert!(backlog.calls().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_drain_that_commits_nothing_admits_without_recounting() -> Result<(), ProjectError> {
        let backlog = Scripted::new(&[OVER_BOUND], false);
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&backlog)).await?;
        assert_eq!(admission, BacklogAdmission::Admit);
        assert_eq!(backlog.calls(), ["count", "drain"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_progressing_drain_defers_only_while_the_backlog_stays_over_bound()
    -> Result<(), ProjectError> {
        let stuck = Scripted::new(&[OVER_BOUND, OVER_BOUND], true);
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&stuck)).await?;
        assert_eq!(admission, BacklogAdmission::Defer);
        assert_eq!(stuck.calls(), ["count", "drain", "count"]);

        let cleared = Scripted::new(&[OVER_BOUND, AUTOMATIC_TERMINAL_BACKLOG_LIMIT], true);
        let admission =
            admit_index_attempt(IndexFailureRetention::AutomaticFailures, Some(&cleared)).await?;
        assert_eq!(admission, BacklogAdmission::Admit);
        Ok(())
    }
}
