//! Axum server setup: router, state, middleware.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;

use crate::native::NativeClient;
use crate::routes;

#[derive(Clone)]
pub enum ProxyMode {
    Subprocess,
    Native,
}

#[derive(Clone)]
pub struct AppState {
    pub cwd: String,
    pub max_turns: u32,
    pub replace_system_prompt: bool,
    pub effort: Option<String>,
    pub embed_system_prompt: bool,
    // Native mode
    pub mode: ProxyMode,
    pub native_client: Option<Arc<NativeClient>>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/v1/models", get(routes::models))
        .route("/v1/messages", post(routes::messages))
        .route("/v1/chat/completions", post(routes::chat_completions))
        .fallback(routes::fallback)
        .layer(CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state)
}
