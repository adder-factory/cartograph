//! Source-free accounting for optional model decisions.

use serde::Serialize;

/// How one decision request obtained its validated answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionCacheOutcome {
    /// This caller performed the provider request and populated the cache.
    Miss,
    /// An unexpired response matched the exact scoped request.
    Hit,
    /// Another caller performed the same request while this caller waited.
    Coalesced,
    /// No project scope or cache capacity was available.
    Bypassed,
}

/// Accounting for one successful decision, without prompts or credentials.
///
/// Token counts describe only this caller's provider work. Cache hits and
/// coalesced waiters have zero provider requests and no billed token counts.
/// Missing counts on a provider response remain unknown, rather than zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionRequestMetrics {
    /// Cache lookup outcome for this caller.
    pub cache: DecisionCacheOutcome,
    /// Total time including credentials, cache waiting, and HTTP admission.
    pub elapsed_ms: u64,
    /// HTTP attempts, including a possible credential-renewal attempt.
    pub provider_requests: u32,
    /// Billable input tokens reported by this caller's successful response.
    pub input_tokens: Option<u64>,
    /// Output tokens reported by this caller's successful response.
    pub output_tokens: Option<u64>,
    /// Whether uncached provider work used background admission.
    pub background: bool,
}
