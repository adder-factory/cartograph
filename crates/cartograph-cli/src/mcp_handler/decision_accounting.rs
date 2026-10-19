use cartograph_domain::{DecisionCacheOutcome, DecisionRequestMetrics};
use serde::Serialize;

const REQUEST_SAMPLE_LIMIT: usize = 64;

/// Bounded accounting for the validated responses observed by a bulk sweep.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DecisionAccounting {
    observed_decisions: u64,
    provider_requests: u64,
    cache: CacheCounts,
    input_tokens: TokenTotals,
    output_tokens: TokenTotals,
    sample_limit: usize,
    omitted_samples: u64,
    samples: Vec<DecisionRequestMetrics>,
}

impl Default for DecisionAccounting {
    fn default() -> Self {
        Self {
            observed_decisions: 0,
            provider_requests: 0,
            cache: CacheCounts::default(),
            input_tokens: TokenTotals::default(),
            output_tokens: TokenTotals::default(),
            sample_limit: REQUEST_SAMPLE_LIMIT,
            omitted_samples: 0,
            samples: Vec::new(),
        }
    }
}

impl DecisionAccounting {
    pub(super) fn extend(&mut self, requests: impl IntoIterator<Item = DecisionRequestMetrics>) {
        for request in requests {
            self.observed_decisions = self.observed_decisions.saturating_add(1);
            self.provider_requests = self
                .provider_requests
                .saturating_add(u64::from(request.provider_requests));
            let count = match request.cache {
                DecisionCacheOutcome::Miss => &mut self.cache.miss,
                DecisionCacheOutcome::Hit => &mut self.cache.hit,
                DecisionCacheOutcome::Coalesced => &mut self.cache.coalesced,
                DecisionCacheOutcome::Bypassed => &mut self.cache.bypassed,
            };
            *count = count.saturating_add(1);
            // Reuse has no provider response whose usage could be unknown.
            if request.provider_requests > 0 {
                self.input_tokens.record(request.input_tokens);
                self.output_tokens.record(request.output_tokens);
            }
            if self.samples.len() < REQUEST_SAMPLE_LIMIT {
                self.samples.push(request);
            } else {
                self.omitted_samples = self.omitted_samples.saturating_add(1);
            }
        }
    }
}

#[derive(Default, Serialize)]
struct CacheCounts {
    miss: u64,
    hit: u64,
    coalesced: u64,
    bypassed: u64,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenTotals {
    known_total: u64,
    responses_with_usage: u64,
    responses_without_usage: u64,
    total_saturated: bool,
}

impl TokenTotals {
    fn record(&mut self, tokens: Option<u64>) {
        if let Some(tokens) = tokens {
            self.responses_with_usage = self.responses_with_usage.saturating_add(1);
            if let Some(total) = self.known_total.checked_add(tokens) {
                self.known_total = total;
            } else {
                self.known_total = u64::MAX;
                self.total_saturated = true;
            }
        } else {
            self.responses_without_usage = self.responses_without_usage.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DecisionRequestMetrics {
        DecisionRequestMetrics {
            cache: DecisionCacheOutcome::Miss,
            elapsed_ms: 400,
            provider_requests: 1,
            input_tokens: Some(120),
            output_tokens: Some(12),
            background: true,
        }
    }

    #[test]
    fn maximum_role_sweep_keeps_usage_report_bounded_and_totals_complete() {
        let mut accounting = DecisionAccounting::default();
        // One response for every 24 symbols at the accepted million-symbol cap.
        let responses = 1_000_000_u64.div_ceil(24);
        accounting.extend((0..responses).map(|_| request()));
        let report = serde_json::to_vec(&accounting)
            .unwrap_or_else(|error| panic!("accounting report serialization failed: {error}"));
        assert!(
            report.len() < 32 * 1024,
            "usage report: {} bytes",
            report.len()
        );
        assert_eq!(accounting.observed_decisions, 41_667);
        assert_eq!(accounting.provider_requests, 41_667);
        assert_eq!(accounting.cache.miss, 41_667);
        assert_eq!(accounting.samples.len(), 64);
        assert_eq!(accounting.omitted_samples, 41_603);
        assert_eq!(accounting.input_tokens.known_total, 5_000_040);
        assert_eq!(accounting.output_tokens.known_total, 500_004);
        assert_eq!(accounting.input_tokens.responses_without_usage, 0);
    }

    #[test]
    fn unknown_provider_usage_is_distinct_from_reuse_and_overflow_is_explicit() {
        let mut unknown = request();
        unknown.input_tokens = None;
        unknown.output_tokens = Some(0);
        let mut hit = unknown;
        hit.cache = DecisionCacheOutcome::Hit;
        hit.provider_requests = 0;
        hit.output_tokens = None;
        let mut huge = request();
        huge.input_tokens = Some(u64::MAX);
        let mut accounting = DecisionAccounting::default();
        accounting.extend([unknown, hit, huge, request()]);
        assert_eq!(accounting.provider_requests, 3);
        assert_eq!(accounting.cache.hit, 1);
        assert_eq!(accounting.input_tokens.responses_without_usage, 1);
        assert_eq!(accounting.input_tokens.responses_with_usage, 2);
        assert_eq!(accounting.input_tokens.known_total, u64::MAX);
        assert!(accounting.input_tokens.total_saturated);
        assert_eq!(accounting.output_tokens.responses_with_usage, 3);
        assert_eq!(accounting.output_tokens.responses_without_usage, 0);
        assert_eq!(accounting.output_tokens.known_total, 24);
        assert!(!accounting.output_tokens.total_saturated);
    }
}
