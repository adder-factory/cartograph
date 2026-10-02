//! Batched Jev decisions shared by the decision surfaces.
//!
//! Items are judged in fixed-size batches with a bounded number of requests
//! in flight, and results are kept in item order. Each batch is retried once
//! for a transient failure. Judging stops at the first failure that means the
//! provider is unavailable; a failure tied to one batch is returned for the
//! caller to decide on.

use std::{collections::BTreeMap, time::Duration};

use cartograph_llm::{JevDecision, JevError, JevQuestion};
use futures_util::{StreamExt as _, stream::FuturesOrdered};
use serde_json::Value;

use crate::navigation::DecisionProvider;

/// Requests in flight for one item list.
const DECISION_CONCURRENCY: usize = 4;
/// Pause before retrying a rate-limited batch once.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(1);

/// One decision surface's request shape.
pub(crate) trait BatchRequest<T> {
    /// Answer read back for each item.
    type Answer;

    /// Provider state and questions for one batch.
    fn build(&self, batch: &[T]) -> (Value, BTreeMap<String, JevQuestion>);

    /// One answer per batch item, or an error for a malformed answer set.
    fn read(&self, decision: &JevDecision, count: usize) -> Result<Vec<Self::Answer>, JevError>;
}

/// Per-batch results in item order, up to where judging stopped.
pub(crate) struct BatchOutcomes<A> {
    /// Each batch's size and its answers or batch-specific failure.
    pub(crate) batches: Vec<(usize, Result<Vec<A>, JevError>)>,
    /// Unavailability that ended judging; later items were not judged.
    pub(crate) stopped: Option<JevError>,
}

/// Failures tied to the batch itself; anything else means the provider is
/// unavailable and judging stops.
pub(crate) const fn rejects_the_batch(error: &JevError) -> bool {
    matches!(
        error,
        JevError::InvalidResponse
            | JevError::BackendRejected
            | JevError::RequestLimit
            | JevError::ResponseLimit
    )
}

/// One decision surface's items and how they are split into requests.
pub(crate) struct BatchInput<'a, T, R> {
    /// Request shape that builds and reads every batch.
    pub(crate) request: &'a R,
    /// Items to judge; results keep this order.
    pub(crate) items: &'a [T],
    /// Items judged per request; zero is treated as one.
    pub(crate) size: usize,
}

/// Judge `input.items` in item order with at most [`DECISION_CONCURRENCY`]
/// requests in flight.
pub(crate) async fn decide_batches<T, R>(
    provider: &impl DecisionProvider,
    input: BatchInput<'_, T, R>,
) -> BatchOutcomes<R::Answer>
where
    R: BatchRequest<T>,
{
    let BatchInput {
        request,
        items,
        size,
    } = input;
    let mut batches = items.chunks(size.max(1));
    let mut in_flight = FuturesOrdered::new();
    for batch in batches.by_ref().take(DECISION_CONCURRENCY) {
        in_flight.push_back(decide_batch(provider, request, batch));
    }
    let mut outcomes = BatchOutcomes {
        batches: Vec::new(),
        stopped: None,
    };
    while let Some((count, decided)) = in_flight.next().await {
        match decided {
            Err(error) if !rejects_the_batch(&error) => {
                outcomes.stopped = Some(error);
                break;
            }
            decided => outcomes.batches.push((count, decided)),
        }
        if let Some(batch) = batches.next() {
            in_flight.push_back(decide_batch(provider, request, batch));
        }
    }
    outcomes
}

/// One retry absorbs a transient failure: about one sweep request in a
/// hundred returned an answer set that failed validation and then passed on
/// replay. A second failure is returned as is.
async fn decide_batch<T, R>(
    provider: &impl DecisionProvider,
    request: &R,
    batch: &[T],
) -> (usize, Result<Vec<R::Answer>, JevError>)
where
    R: BatchRequest<T>,
{
    let decided = match decide_once(provider, request, batch).await {
        Err(
            error @ (JevError::InvalidResponse
            | JevError::EndpointUnavailable
            | JevError::RateLimited),
        ) => {
            if error == JevError::RateLimited {
                tokio::time::sleep(RATE_LIMIT_PAUSE).await;
            }
            decide_once(provider, request, batch).await
        }
        decided => decided,
    };
    (batch.len(), decided)
}

async fn decide_once<T, R>(
    provider: &impl DecisionProvider,
    request: &R,
    batch: &[T],
) -> Result<Vec<R::Answer>, JevError>
where
    R: BatchRequest<T>,
{
    let (state, questions) = request.build(batch);
    let decision = provider.decide(&state, &questions).await?;
    request.read(&decision, batch.len())
}

/// `text` cut to at most `limit` bytes on a character boundary.
pub(crate) fn truncated(text: &str, limit: usize) -> &str {
    &text[..text.floor_char_boundary(limit)]
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use cartograph_llm::{JEV_MODEL, JevAnswer};
    use serde_json::json;

    use super::*;

    /// Answers each item with its own value as a probability.
    struct Echo;

    impl BatchRequest<u32> for Echo {
        type Answer = f64;

        fn build(&self, batch: &[u32]) -> (Value, BTreeMap<String, JevQuestion>) {
            let questions = (0..batch.len())
                .map(|index| {
                    (
                        format!("q_{index}"),
                        JevQuestion::Noul {
                            instructions: "fixture".to_owned(),
                            criteria: None,
                        },
                    )
                })
                .collect();
            (json!({ "items": batch }), questions)
        }

        fn read(&self, decision: &JevDecision, count: usize) -> Result<Vec<f64>, JevError> {
            (0..count)
                .map(|index| match decision.answers.get(&format!("q_{index}")) {
                    Some(JevAnswer::Noul { noul }) => Ok(*noul),
                    _ => Err(JevError::InvalidResponse),
                })
                .collect()
        }
    }

    /// Answers `item / 100`. A batch whose first item is listed fails with
    /// that error the listed number of times before answering. Records batch
    /// sizes and the most requests in flight at once.
    struct Provider {
        calls: AtomicUsize,
        failing: Mutex<Vec<(u32, JevError, usize)>>,
        in_flight: AtomicUsize,
        most_in_flight: AtomicUsize,
        sizes: Mutex<Vec<usize>>,
    }

    impl Provider {
        fn failing(failing: Vec<(u32, JevError, usize)>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                failing: Mutex::new(failing),
                in_flight: AtomicUsize::new(0),
                most_in_flight: AtomicUsize::new(0),
                sizes: Mutex::new(Vec::new()),
            }
        }

        fn failure_for(&self, first: u32) -> Option<JevError> {
            let mut failing = self
                .failing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = failing
                .iter_mut()
                .find(|(item, _, remaining)| *item == first && *remaining > 0)?;
            entry.2 -= 1;
            Some(entry.1.clone())
        }
    }

    impl DecisionProvider for Provider {
        async fn decide(
            &self,
            state: &Value,
            questions: &BTreeMap<String, JevQuestion>,
        ) -> Result<JevDecision, JevError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_in_flight.fetch_max(now, Ordering::SeqCst);
            self.sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(questions.len());
            tokio::task::yield_now().await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            let first = state["items"][0]
                .as_u64()
                .and_then(|item| u32::try_from(item).ok())
                .unwrap_or(u32::MAX);
            if let Some(error) = self.failure_for(first) {
                return Err(error);
            }
            let answers = state["items"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, item)| {
                    let noul = item.as_f64().unwrap_or_default() / 100.0;
                    (format!("q_{index}"), JevAnswer::Noul { noul })
                })
                .collect();
            Ok(JevDecision {
                model: JEV_MODEL.to_owned(),
                answers,
            })
        }
    }

    /// `items` judged by [`Echo`] in batches of `size`.
    fn echo(items: &[u32], size: usize) -> BatchInput<'_, u32, Echo> {
        BatchInput {
            request: &Echo,
            items,
            size,
        }
    }

    fn answers(outcomes: &BatchOutcomes<f64>) -> Vec<Option<f64>> {
        outcomes
            .batches
            .iter()
            .flat_map(|(size, decided)| match decided {
                Ok(answers) => answers.iter().copied().map(Some).collect::<Vec<_>>(),
                Err(_) => vec![None; *size],
            })
            .collect()
    }

    #[tokio::test]
    async fn batches_keep_item_order_and_cap_requests_in_flight() {
        let items = (0..50).collect::<Vec<u32>>();
        let provider = Provider::failing(Vec::new());
        let outcomes = decide_batches(&provider, echo(&items, 24)).await;
        assert_eq!(outcomes.stopped, None);
        let expected = items
            .iter()
            .map(|item| Some(f64::from(*item) / 100.0))
            .collect::<Vec<_>>();
        assert_eq!(answers(&outcomes), expected);
        let mut sizes = provider
            .sizes
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sizes.sort_unstable();
        assert_eq!(sizes, [2, 24, 24]);
        assert!(provider.most_in_flight.load(Ordering::SeqCst) <= DECISION_CONCURRENCY);

        let many = (0..200).collect::<Vec<u32>>();
        let busy = Provider::failing(Vec::new());
        decide_batches(&busy, echo(&many, 1)).await;
        let most = busy.most_in_flight.load(Ordering::SeqCst);
        assert!(
            (2..=DECISION_CONCURRENCY).contains(&most),
            "{most} requests in flight"
        );
    }

    #[tokio::test]
    async fn a_transient_failure_retries_and_a_rejection_does_not_stop_later_batches() {
        let items = (0..3).collect::<Vec<u32>>();
        // The first batch fails once and its retry succeeds.
        let recovered = Provider::failing(vec![(0, JevError::EndpointUnavailable, 1)]);
        let outcomes = decide_batches(&recovered, echo(&items, 1)).await;
        assert_eq!(answers(&outcomes), [Some(0.0), Some(0.01), Some(0.02)]);
        assert_eq!(recovered.calls.load(Ordering::SeqCst), 4);

        // The second item's batch is rejected twice; the third is still judged.
        let rejected = Provider::failing(vec![(1, JevError::BackendRejected, 2)]);
        let outcomes = decide_batches(&rejected, echo(&items, 1)).await;
        assert_eq!(outcomes.stopped, None);
        assert!(matches!(
            outcomes.batches[1].1,
            Err(JevError::BackendRejected)
        ));
        assert_eq!(answers(&outcomes)[2], Some(0.02));
    }

    #[tokio::test]
    async fn unavailability_stops_at_its_batch_and_keeps_the_prefix() {
        let items = (0..6).collect::<Vec<u32>>();
        // The second batch starts at item 2 and is unavailable on both attempts.
        let unavailable = Provider::failing(vec![(2, JevError::EndpointUnavailable, 2)]);
        let outcomes = decide_batches(&unavailable, echo(&items, 2)).await;
        assert_eq!(outcomes.stopped, Some(JevError::EndpointUnavailable));
        assert_eq!(answers(&outcomes), [Some(0.0), Some(0.01)]);
    }
}
