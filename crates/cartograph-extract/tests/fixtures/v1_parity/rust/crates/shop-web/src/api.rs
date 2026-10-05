use axum::routing::{get, post};
use axum::Router;
use crate::handlers::orders::ping_handler;

pub const API_VERSION: u32 = 2;

pub fn api_routes() -> Router {
    Router::new()
        .route("/users", get(list_users))
        .route ("/users/new", post(create_user))
        .route("/ping", get(ping_handler))
}

async fn list_users() -> String {
    format!("v{}", API_VERSION)
}

async fn create_user() -> u32 {
    API_VERSION
}

#[tauri::command]
fn desktop_bridge() -> u32 {
    version()
}

fn version() -> u32 {
    API_VERSION
}
