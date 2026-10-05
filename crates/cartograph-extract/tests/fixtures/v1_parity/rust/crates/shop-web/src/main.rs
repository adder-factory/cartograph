extern crate shop_core;

mod handlers;
mod services;
mod api;

use actix_web::{get, post, web, App, HttpServer, Responder};
use axum::{routing::get as axum_get, routing::post as axum_post, Router};
use shop_core;
use shop_core::models::Order;
use crate::handlers::orders::{ping_handler, handle_checkout};
use crate::services::OrderService;

#[get("/health")]
async fn health() -> impl Responder {
    "ok"
}

#[get("/a")]
#[post("/b")]
async fn multi() -> impl Responder {
    "multi"
}

#[head("/h")]
async fn head_probe() {}

#[options("/o")]
async fn options_probe() {}

#[allow(dead_code)]
fn unused_helper() {}

fn router() -> Router {
    Router::new()
        .route("/ping", axum_get(ping_handler))
        .route ("/checkout", axum_post(handle_checkout))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let svc = OrderService::new();
    let order: Order = svc.place(3);
    println!("{}", order.total());
    let _r = router();
    let _a = api::api_routes();
    let _limit = shop_core::limit();
    HttpServer::new(|| App::new().service(health).service(multi))
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
