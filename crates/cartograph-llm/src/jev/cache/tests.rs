use super::*;

fn leader(cache: &Arc<DecisionCache>, key: [u8; 32]) -> FlightOwner {
    let Access::Lead(owner) = cache.access(key, Instant::now()) else {
        panic!("expected producer");
    };
    owner
}

#[tokio::test]
async fn successful_flight_is_shared_and_then_cached() {
    let cache = Arc::new(DecisionCache::default());
    let owner = leader(&cache, [1; 32]);
    let Access::Wait(waiter) = cache.access([1; 32], Instant::now()) else {
        panic!("expected waiter");
    };
    owner.finish(Ok(Arc::from(b"answer".as_slice())), true);
    assert_eq!(waiter.wait().await.unwrap_or_default().as_ref(), b"answer");
    let Access::Hit(response) = cache.access([1; 32], Instant::now()) else {
        panic!("expected cache hit");
    };
    assert_eq!(response.as_ref(), b"answer");
}

#[tokio::test]
async fn failure_is_shared_but_never_cached() {
    let cache = Arc::new(DecisionCache::default());
    let owner = leader(&cache, [2; 32]);
    let Access::Wait(waiter) = cache.access([2; 32], Instant::now()) else {
        panic!("expected waiter");
    };
    owner.finish(Err(JevError::AuthenticationFailed), true);
    assert_eq!(waiter.wait().await, Err(JevError::AuthenticationFailed));
    drop(leader(&cache, [2; 32]));
}

#[tokio::test]
async fn cancelled_producer_releases_waiters_and_the_flight_slot() {
    let cache = Arc::new(DecisionCache::default());
    let owner = leader(&cache, [3; 32]);
    let Access::Wait(waiter) = cache.access([3; 32], Instant::now()) else {
        panic!("expected waiter");
    };
    drop(owner);
    assert_eq!(waiter.wait().await, Err(JevError::EndpointUnavailable));
    drop(leader(&cache, [3; 32]));
}

#[test]
fn flights_and_waiters_have_independent_capacity_limits() {
    let cache = Arc::new(DecisionCache::default());
    let mut owners = Vec::new();
    for key in 0..MAXIMUM_FLIGHTS {
        owners.push(leader(&cache, [u8::try_from(key).unwrap_or_default(); 32]));
    }
    assert!(matches!(
        cache.access([255; 32], Instant::now()),
        Access::Bypass
    ));
    let mut waiters = Vec::new();
    for _ in 0..MAXIMUM_WAITERS {
        let Access::Wait(waiter) = cache.access([0; 32], Instant::now()) else {
            panic!("expected waiter");
        };
        waiters.push(waiter);
    }
    assert!(matches!(
        cache.access([0; 32], Instant::now()),
        Access::Bypass
    ));
    drop(waiters);
    assert!(matches!(
        cache.access([0; 32], Instant::now()),
        Access::Wait(_)
    ));
    drop(owners);
    drop(leader(&cache, [255; 32]));
}

#[test]
fn byte_and_entry_limits_and_expiry_are_enforced() {
    let now = Instant::now();
    let mut state = State::default();
    for key in 0..300_u16 {
        let mut identity = [0; 32];
        identity[..2].copy_from_slice(&key.to_le_bytes());
        state.insert(identity, Arc::from(b"x".as_slice()), now);
    }
    assert_eq!(state.entries.len(), MAXIMUM_ENTRIES);
    assert_eq!(state.bytes, MAXIMUM_ENTRIES);
    for key in 0..10_u8 {
        state.insert([key; 32], vec![0; 512 * 1024].into(), now);
    }
    assert_eq!(state.bytes, MAXIMUM_BYTES);
    assert_eq!(state.entries.len(), 8);
    state.insert([255; 32], vec![0; MAXIMUM_BYTES + 1].into(), now);
    assert_eq!(state.entries.len(), 8);
    state.expire(now + LIFETIME);
    assert_eq!(state.bytes, 0);
    assert!(state.entries.is_empty());
}

#[test]
fn reading_a_cached_entry_does_not_extend_its_lifetime() {
    let cache = Arc::new(DecisionCache::default());
    let now = Instant::now();
    cache
        .state
        .lock()
        .unwrap_or_else(|e| panic!("lock: {e}"))
        .insert([7; 32], Arc::from(b"x".as_slice()), now);
    assert!(matches!(
        cache.access([7; 32], now + LIFETIME / 2),
        Access::Hit(_)
    ));
    assert!(matches!(
        cache.access([7; 32], now + LIFETIME),
        Access::Lead(_)
    ));
}

#[test]
fn renewed_credential_response_is_not_stored_under_the_old_identity() {
    let cache = Arc::new(DecisionCache::default());
    leader(&cache, [8; 32]).finish(Ok(Arc::from(b"x".as_slice())), false);
    drop(leader(&cache, [8; 32]));
}
