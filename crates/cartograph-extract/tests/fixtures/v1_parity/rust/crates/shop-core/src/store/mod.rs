use crate::models::{Order, LineItem};
use super::MAX_ITEMS;
use self::inner::Slot;
use std::sync::*;

mod inner {
    pub struct Slot {
        pub n: u32,
    }
}

pub trait Store {
    fn put(&mut self, order: Order) -> bool;
    fn get(&self, id: u64) -> Option<&Order>;
}

pub struct MemoryStore {
    orders: Vec<Order>,
    lock: Mutex<u8>,
    slot: Slot,
}

pub struct StoreBuilder {
    cap: usize,
}

impl StoreBuilder {
    pub fn new() -> StoreBuilder {
        StoreBuilder { cap: MAX_ITEMS }
    }

    pub fn build(self) -> MemoryStore {
        MemoryStore { orders: Vec::with_capacity(self.cap), lock: Mutex::new(0), slot: Slot { n: 0 } }
    }
}

impl Store for MemoryStore {
    fn put(&mut self, order: Order) -> bool {
        if self.orders.len() >= MAX_ITEMS {
            return false;
        }
        self.orders.push(order);
        true
    }

    fn get(&self, id: u64) -> Option<&Order> {
        self.orders.iter().find(|o| o.id == id)
    }
}

pub fn seeded() -> MemoryStore {
    let mut store = StoreBuilder::new().build();
    let mut order = Order::new(1);
    order.add(LineItem { sku: String::from("A1"), qty: 2 });
    store.put(order);
    let parsed = "7".parse::<u32>().unwrap_or_default();
    let _ = std::mem::size_of::<Slot>() + parsed as usize;
    store
}
