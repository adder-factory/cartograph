pub(crate) mod native;

use serde::Serialize;

const MAX_INTENT_TOKENS: usize = 32;
const MAX_INTENT_TOKEN_BYTES: usize = 128;

/// Exact durable queue changes caused by one no-hit intent lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPriorityEnqueueReport {
    candidates: u64,
    enqueued: u64,
    refreshed: u64,
}

impl SummaryPriorityEnqueueReport {
    #[must_use]
    /// Returns the candidates.
    pub const fn candidates(&self) -> u64 {
        self.candidates
    }

    #[must_use]
    /// Returns the enqueued.
    pub const fn enqueued(&self) -> u64 {
        self.enqueued
    }

    #[must_use]
    /// Returns the refreshed.
    pub const fn refreshed(&self) -> u64 {
        self.refreshed
    }

    #[must_use]
    /// Returns the affected.
    pub const fn affected(&self) -> u64 {
        self.enqueued.saturating_add(self.refreshed)
    }
}

/// Current-generation demand-driven summary backlog health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPriorityQueueStats {
    pending: u64,
    oldest_enqueued_at: Option<String>,
    newest_enqueued_at: Option<String>,
    total_requests: u64,
    attempted: u64,
}

impl SummaryPriorityQueueStats {
    #[must_use]
    /// Returns the pending.
    pub const fn pending(&self) -> u64 {
        self.pending
    }
}

/// Result of one poison-row attempt increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPriorityFailure {
    present: bool,
    evicted: bool,
}

impl SummaryPriorityFailure {
    #[must_use]
    /// Returns the evicted.
    pub const fn evicted(self) -> bool {
        self.evicted
    }
}

fn validate_tokens(tokens: &[String]) -> Result<(), StorageError> {
    if tokens.is_empty()
        || tokens.len() > MAX_INTENT_TOKENS
        || tokens.iter().any(|token| {
            token.len() < 3
                || token.len() > MAX_INTENT_TOKEN_BYTES
                || token.chars().any(char::is_control)
        })
    {
        Err(StorageError::InvalidInput {
            field: "summary_priority_tokens",
        })
    } else {
        Ok(())
    }
}

use crate::StorageError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_terms_and_limits_are_bounded_before_database_work() {
        assert!(validate_tokens(&["parse".to_owned(), "token".to_owned()]).is_ok());
        assert!(validate_tokens(&[]).is_err());
        assert!(validate_tokens(&["xy".to_owned()]).is_err());
        assert!(validate_tokens(&["bad\nterm".to_owned()]).is_err());
        assert!(validate_tokens(&vec!["token".to_owned(); MAX_INTENT_TOKENS + 1]).is_err());
    }
}
