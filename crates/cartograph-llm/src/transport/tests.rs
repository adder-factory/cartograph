use std::assert_matches;
use std::{
    future::Future as _,
    pin::pin,
    task::{Context, Poll, Waker},
};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{
    Arc, Duration, MAXIMUM_ACTIVE_REQUESTS, MAXIMUM_BACKGROUND_REQUESTS, MAXIMUM_ENDPOINTS,
    MAXIMUM_TRANSPORTS, MAXIMUM_WAITING_BACKGROUND_REQUESTS, MAXIMUM_WAITING_REQUESTS,
    RequestPriority, TierCredential, TransportRegistry, TransportSettings, Url,
};
use crate::CredentialCommand;
use secrecy::SecretString;

fn settings<'a>(endpoint: &'a Url, credential: &'a TierCredential) -> TransportSettings<'a> {
    TransportSettings {
        endpoint,
        model: "fixture",
        credential,
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn repeated_construction_reuses_a_real_keep_alive_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("transport listener failed: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("transport address failed: {error}"));
    let endpoint = Url::parse(&format!("http://{address}/v1/embeddings"))
        .unwrap_or_else(|error| panic!("transport endpoint failed: {error}"));
    let mut registry = TransportRegistry::default();
    let client = async {
        for _ in 0..2 {
            let transport = registry
                .get(settings(&endpoint, &TierCredential::None))
                .unwrap_or_else(|()| panic!("transport construction failed"));
            let response = transport
                .client
                .get(endpoint.clone())
                .send()
                .await
                .unwrap_or_else(|error| panic!("transport request failed: {error}"));
            assert_eq!(response.text().await.ok().as_deref(), Some("ok"));
        }
    };
    let server = async {
        let (mut connection, _) = listener
            .accept()
            .await
            .unwrap_or_else(|error| panic!("transport accept failed: {error}"));
        for _ in 0..2 {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 4096);
                request.push(
                    connection
                        .read_u8()
                        .await
                        .unwrap_or_else(|error| panic!("transport header failed: {error}")),
                );
            }
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap_or_else(|error| panic!("transport response failed: {error}"));
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(client, server);
    })
    .await
    .unwrap_or_else(|error| panic!("connection reuse failed: {error}"));
    assert_eq!(registry.transports.len(), 1);
}

#[test]
fn settings_and_secret_rotation_replace_transport_without_replacing_endpoint_capacity() {
    let endpoint = Url::parse("http://127.0.0.1:18083/v1/embeddings")
        .unwrap_or_else(|error| panic!("transport endpoint failed: {error}"));
    let secret_a = TierCredential::Static(SecretString::from("fixture-a"));
    let secret_b = TierCredential::Static(SecretString::from("fixture-b"));
    let command = TierCredential::Command(
        CredentialCommand::new(vec!["fixture-a".to_owned()])
            .unwrap_or_else(|error| panic!("credential command failed: {error}")),
    );
    let mut registry = TransportRegistry::default();
    let original = registry
        .get(settings(&endpoint, &secret_a))
        .unwrap_or_else(|()| panic!("transport construction failed"));
    let repeated = registry
        .get(settings(&endpoint, &secret_a))
        .unwrap_or_else(|()| panic!("transport repeat failed"));
    assert!(Arc::ptr_eq(&original, &repeated));
    for mutation in 0..4 {
        let mut changed = settings(&endpoint, &secret_a);
        match mutation {
            0 => changed.credential = &secret_b,
            1 => changed.model = "replacement",
            2 => changed.credential = &command,
            _ => changed.request_timeout = Duration::from_secs(3),
        }
        let changed = registry
            .get(changed)
            .unwrap_or_else(|()| panic!("transport rotation failed"));
        assert!(!Arc::ptr_eq(&original, &changed));
        assert!(Arc::ptr_eq(&original.admission, &changed.admission));
    }
}

#[tokio::test]
async fn background_load_leaves_foreground_capacity_and_cancelled_waiters_release_admission() {
    let endpoint = Url::parse("http://127.0.0.1:18083/v1/embeddings")
        .unwrap_or_else(|error| panic!("transport endpoint failed: {error}"));
    let transport = TransportRegistry::default()
        .get(settings(&endpoint, &TierCredential::None))
        .unwrap_or_else(|()| panic!("transport construction failed"));
    let mut background = Vec::new();
    for _ in 0..MAXIMUM_BACKGROUND_REQUESTS {
        background.push(
            transport
                .admit(RequestPriority::Background, Duration::from_secs(5))
                .await
                .unwrap_or_else(|()| panic!("background admission failed")),
        );
    }
    let foreground = transport
        .admit(RequestPriority::Foreground, Duration::from_secs(5))
        .await
        .unwrap_or_else(|()| panic!("foreground capacity was unavailable"));
    assert_eq!(transport.admission.active.available_permits(), 0);
    let mut pending = Vec::new();
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..MAXIMUM_WAITING_BACKGROUND_REQUESTS {
        let mut waiter =
            Box::pin(transport.admit(RequestPriority::Background, Duration::from_secs(5)));
        assert_matches!(
            waiter.as_mut().poll(&mut context).map_ok(drop),
            Poll::Pending
        );
        pending.push(waiter);
    }
    assert!(
        transport
            .admit(RequestPriority::Background, Duration::from_secs(5))
            .await
            .is_err()
    );
    drop(foreground);
    let foreground = transport
        .admit(RequestPriority::Foreground, Duration::from_secs(5))
        .await
        .unwrap_or_else(|()| panic!("background waiters blocked foreground admission"));
    assert!(
        transport
            .admit(RequestPriority::Foreground, Duration::from_millis(1))
            .await
            .is_err()
    );
    drop(pending);
    assert_eq!(
        transport.admission.waiting.available_permits(),
        MAXIMUM_WAITING_REQUESTS
    );
    assert_eq!(
        transport.admission.waiting_background.available_permits(),
        MAXIMUM_WAITING_BACKGROUND_REQUESTS
    );
    drop(foreground);
    drop(background);
    assert_eq!(
        transport.admission.active.available_permits(),
        MAXIMUM_ACTIVE_REQUESTS
    );
    let mut cancelled = pin!(transport.admit(RequestPriority::Foreground, Duration::from_secs(5)));
    assert_matches!(
        cancelled.as_mut().poll(&mut context).map_ok(drop),
        Poll::Ready(Ok(()))
    );
}

#[test]
fn bounded_registry_cannot_replace_capacity_still_owned_by_active_clients() {
    let mut registry = TransportRegistry::default();
    let mut retained = Vec::new();
    for index in 0..MAXIMUM_ENDPOINTS {
        let endpoint = Url::parse(&format!("http://127.0.0.1:{}/v1/embeddings", 20000 + index))
            .unwrap_or_else(|error| panic!("transport endpoint failed: {error}"));
        retained.push(
            registry
                .get(settings(&endpoint, &TierCredential::None))
                .unwrap_or_else(|()| panic!("transport construction failed")),
        );
    }
    let overflow = Url::parse("http://127.0.0.1:30000/v1/embeddings")
        .unwrap_or_else(|error| panic!("transport endpoint failed: {error}"));
    assert!(
        registry
            .get(settings(&overflow, &TierCredential::None))
            .is_err()
    );
    assert!(registry.transports.len() <= MAXIMUM_TRANSPORTS);
    assert_eq!(registry.endpoints.len(), MAXIMUM_ENDPOINTS);
    drop(retained);
    assert!(
        registry
            .get(settings(&overflow, &TierCredential::None))
            .is_ok()
    );
}
