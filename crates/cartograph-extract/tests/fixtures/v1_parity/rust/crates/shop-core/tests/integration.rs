use shop_core::store::{seeded, Store};

#[test]
fn seeded_store_has_order() {
    let store = seeded();
    assert!(store.get(1).is_some());
}
