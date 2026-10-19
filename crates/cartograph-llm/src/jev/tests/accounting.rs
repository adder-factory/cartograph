use cartograph_domain::DecisionCacheOutcome;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

fn client(endpoint: Url, scope: u8, key: &str) -> JevClient {
    let mut settings = settings_for(
        endpoint,
        TierCredential::Static(SecretString::from(key.to_owned())),
    );
    settings.cache_scope = Some([scope; 32]);
    JevClient::new(settings).unwrap_or_else(|e| panic!("client: {e}"))
}

fn with_usage() -> String {
    let mut response = valid_response();
    response["usage"] = serde_json::json!({"input_tokens":120,"output_tokens":12});
    response.to_string()
}

/// Bounded async HTTP fixture; `join!` owns both server and requesting futures.
async fn serve(listener: tokio::net::TcpListener, replies: Vec<String>) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener
                .accept()
                .await
                .unwrap_or_else(|e| panic!("accept: {e}"));
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = stream
                    .read(&mut chunk)
                    .await
                    .unwrap_or_else(|e| panic!("read: {e}"));
                assert!(count > 0 && bytes.len() < MAXIMUM_REQUEST_BYTES);
                bytes.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find(HEADER_TERMINATOR) {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")?
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or_default();
                    if bytes.len() >= end + HEADER_TERMINATOR.len() + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap_or_else(|e| panic!("UTF8: {e}")));
            stream
                .write_all(reply.as_bytes())
                .await
                .unwrap_or_else(|e| panic!("write: {e}"));
        }
        requests
    })
    .await
    .unwrap_or_else(|e| panic!("provider fixture deadline: {e}"))
}

async fn endpoint() -> (Url, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("listen: {e}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    (
        Url::parse(&format!("http://{address}/v1/systemone"))
            .unwrap_or_else(|e| panic!("URL: {e}")),
        listener,
    )
}

#[tokio::test]
async fn cache_hits_do_not_bill_twice_and_state_scope_key_and_questions_invalidate() {
    let (endpoint, listener) = endpoint().await;
    let first = client(endpoint.clone(), 1, "fixture-key-a");
    let another_project = client(endpoint.clone(), 2, "fixture-key-a");
    let another_key = client(endpoint, 1, "fixture-key-b");
    let q = questions();
    let run = async {
        let request = |state| first.decide(state, &q);
        let initial = request(&Value::Null)
            .await
            .unwrap_or_else(|e| panic!("first: {e}"));
        let metrics = initial.metrics.unwrap_or_else(|| panic!("missing metrics"));
        assert_eq!(metrics.cache, DecisionCacheOutcome::Miss);
        assert_eq!(
            (
                metrics.provider_requests,
                metrics.input_tokens,
                metrics.output_tokens
            ),
            (1, Some(120), Some(12))
        );
        let hit = request(&Value::Null)
            .await
            .unwrap_or_else(|e| panic!("cached: {e}"));
        let metrics = hit.metrics.unwrap_or_else(|| panic!("missing metrics"));
        assert_eq!(metrics.cache, DecisionCacheOutcome::Hit);
        assert_eq!(
            (
                metrics.provider_requests,
                metrics.input_tokens,
                metrics.output_tokens
            ),
            (0, None, None)
        );
        assert!(hit.usage.is_none());
        for decision in [
            first
                .decide(&serde_json::json!({"generation":"new"}), &q)
                .await,
            another_project.decide(&Value::Null, &q).await,
            another_key.decide(&Value::Null, &q).await,
        ] {
            assert_eq!(
                decision
                    .unwrap_or_else(|e| panic!("different identity: {e}"))
                    .metrics
                    .map(|m| m.cache),
                Some(DecisionCacheOutcome::Miss)
            );
        }
        let mut changed = questions();
        if let Some(JevQuestion::Noul { instructions, .. }) = changed.get_mut("sufficient") {
            instructions.push_str(" with a different criterion");
        }
        assert_eq!(
            first
                .decide(&Value::Null, &changed)
                .await
                .unwrap_or_else(|e| panic!("different schema: {e}"))
                .metrics
                .map(|m| m.cache),
            Some(DecisionCacheOutcome::Miss)
        );
    };
    let ((), requests) = tokio::join!(
        run,
        serve(
            listener,
            vec![http_response("200 OK", &with_usage(), ""); 5]
        )
    );
    assert_eq!(requests.len(), 5);
}

#[tokio::test]
async fn simultaneous_identical_requests_share_one_http_call_and_one_usage_record() {
    let (endpoint, listener) = endpoint().await;
    let first = client(endpoint.clone(), 3, "fixture-key");
    let second = client(endpoint, 3, "fixture-key");
    let q = questions();
    let run = async {
        let (a, b) = tokio::join!(
            first.decide(&Value::Null, &q),
            second.decide(&Value::Null, &q)
        );
        let a = a
            .unwrap_or_else(|e| panic!("producer: {e}"))
            .metrics
            .unwrap_or_else(|| panic!("metrics"));
        let b = b
            .unwrap_or_else(|e| panic!("waiter: {e}"))
            .metrics
            .unwrap_or_else(|| panic!("metrics"));
        assert_eq!(
            (a.cache, b.cache),
            (DecisionCacheOutcome::Miss, DecisionCacheOutcome::Coalesced)
        );
        assert_eq!(a.provider_requests + b.provider_requests, 1);
        assert_eq!(a.input_tokens, Some(120));
        assert_eq!(b.input_tokens, None);
    };
    let ((), requests) = tokio::join!(
        run,
        serve(listener, vec![http_response("200 OK", &with_usage(), "")])
    );
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn malformed_response_and_provider_failure_are_never_cached() {
    let (endpoint, listener) = endpoint().await;
    let client = client(endpoint, 4, "fixture-key");
    let q = questions();
    let run = async {
        assert_eq!(
            client.decide(&Value::Null, &q).await.err(),
            Some(JevError::InvalidResponse)
        );
        assert_eq!(
            client.decide(&Value::Null, &q).await.err(),
            Some(JevError::RateLimited)
        );
        let final_call = client
            .decide(&Value::Null, &q)
            .await
            .unwrap_or_else(|e| panic!("retry: {e}"));
        assert_eq!(
            final_call.metrics.map(|m| m.cache),
            Some(DecisionCacheOutcome::Miss)
        );
    };
    let replies = vec![
        http_response("200 OK", "{}", ""),
        http_response("429 Too Many Requests", "", ""),
        http_response("200 OK", &valid_response().to_string(), ""),
    ];
    let ((), requests) = tokio::join!(run, serve(listener, replies));
    assert_eq!(requests.len(), 3);
}

#[tokio::test]
async fn background_client_preserves_an_active_slot_for_foreground_work() {
    let (endpoint, listener) = endpoint().await;
    let background = client(endpoint.clone(), 5, "fixture-key").for_background();
    let foreground = client(endpoint, 6, "fixture-key");
    let mut holds = Vec::new();
    for _ in 0..3 {
        holds.push(
            background
                .transport
                .admit(RequestPriority::Background, Duration::from_secs(1))
                .await
                .unwrap_or_else(|()| panic!("admission")),
        );
    }
    let q = questions();
    let run = async {
        let mut blocked = std::pin::pin!(background.decide(&Value::Null, &q));
        assert!(futures_util::poll!(&mut blocked).is_pending());
        let decision = foreground
            .decide(&Value::Null, &q)
            .await
            .unwrap_or_else(|e| panic!("foreground: {e}"));
        assert_eq!(decision.metrics.map(|m| m.background), Some(false));
        drop(holds);
        let decision = blocked.await.unwrap_or_else(|e| panic!("background: {e}"));
        assert_eq!(decision.metrics.map(|m| m.background), Some(true));
    };
    tokio::join!(
        run,
        serve(
            listener,
            vec![http_response("200 OK", &with_usage(), ""); 2]
        )
    );
}

#[test]
fn usage_is_optional_nonnegative_and_cannot_forge_local_accounting() {
    let q = questions();
    let decode =
        |response: Value| decode_response(&serde_json::to_vec(&response).unwrap_or_default(), &q);
    assert!(
        decode(valid_response())
            .unwrap_or_else(|e| panic!("legacy: {e}"))
            .usage
            .is_none()
    );
    let mut response = valid_response();
    response["usage"] = serde_json::json!({"input_tokens":0});
    response["metrics"] = serde_json::json!({"providerRequests":0});
    let decision = decode(response).unwrap_or_else(|e| panic!("usage: {e}"));
    assert!(decision.metrics.is_none());
    assert_eq!(decision.usage.and_then(|usage| usage.input_tokens), Some(0));
    assert_eq!(decision.usage.and_then(|usage| usage.output_tokens), None);
    for value in [
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("secret"),
    ] {
        let mut response = valid_response();
        response["usage"] = serde_json::json!({"input_tokens":value});
        assert_eq!(decode(response).err(), Some(JevError::InvalidResponse));
    }
}

#[tokio::test]
async fn cancelling_a_queued_producer_releases_coalesced_callers_and_allows_retry() {
    let (endpoint, listener) = endpoint().await;
    let client = client(endpoint, 8, "fixture-key");
    let mut holds = Vec::new();
    for _ in 0..4 {
        holds.push(
            client
                .transport
                .admit(RequestPriority::Foreground, Duration::from_secs(1))
                .await
                .unwrap_or_else(|()| panic!("hold active slot")),
        );
    }
    let q = questions();
    let mut producer = Box::pin(client.decide(&Value::Null, &q));
    assert!(futures_util::poll!(&mut producer).is_pending());
    let mut follower = Box::pin(client.decide(&Value::Null, &q));
    assert!(futures_util::poll!(&mut follower).is_pending());
    drop(producer);
    assert_eq!(follower.await.err(), Some(JevError::EndpointUnavailable));
    drop(holds);
    let run = async {
        let result = client
            .decide(&Value::Null, &q)
            .await
            .unwrap_or_else(|e| panic!("retry: {e}"));
        assert_eq!(
            result.metrics.map(|m| m.cache),
            Some(DecisionCacheOutcome::Miss)
        );
    };
    let ((), requests) = tokio::join!(
        run,
        serve(listener, vec![http_response("200 OK", &with_usage(), "")])
    );
    assert_eq!(
        requests.len(),
        1,
        "cancelled queued work never reached HTTP"
    );
}

#[tokio::test]
async fn a_coalesced_waiter_keeps_its_own_shorter_deadline() {
    let (endpoint, _listener) = endpoint().await;
    let producer = client(endpoint.clone(), 9, "fixture-key");
    let mut follower = client(endpoint, 9, "fixture-key");
    follower.settings.timeout = Duration::from_millis(20);
    let mut holds = Vec::new();
    for _ in 0..4 {
        holds.push(
            producer
                .transport
                .admit(RequestPriority::Foreground, Duration::from_secs(1))
                .await
                .unwrap_or_else(|()| panic!("hold active slot")),
        );
    }
    let q = questions();
    let mut pending = Box::pin(producer.decide(&Value::Null, &q));
    assert!(futures_util::poll!(&mut pending).is_pending());
    let result = tokio::time::timeout(Duration::from_secs(1), follower.decide(&Value::Null, &q))
        .await
        .unwrap_or_else(|e| panic!("waiter ignored its own deadline: {e}"));
    assert_eq!(result.err(), Some(JevError::EndpointUnavailable));
    drop(pending);
    drop(holds);
}
