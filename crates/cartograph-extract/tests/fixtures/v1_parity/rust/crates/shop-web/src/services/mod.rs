use shop_core::models::{LineItem, Order};
use shop_core::store::{Store, StoreBuilder};

pub struct OrderService {
    count: u32,
}

impl OrderService {
    pub fn new() -> Self {
        OrderService { count: 0 }
    }

    pub fn place(&self, qty: u32) -> Order {
        let mut order = Order::new(self.count as u64);
        order.add(LineItem { sku: "B2".to_string(), qty });
        let mut store = StoreBuilder::new().build();
        store.put(order.clone());
        order
    }
}
