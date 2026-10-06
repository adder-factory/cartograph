use crate::services::OrderService;

pub async fn ping_handler() -> &'static str {
    "pong"
}

pub async fn handle_checkout() -> String {
    let svc = OrderService::new();
    let order = svc.place(1);
    order.total().to_string()
}
