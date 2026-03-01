use axum::{
   Json,
   Router,
   extract::State,
   routing::get,
};

use crate::{
   AppState,
   api::{
      DebugResponse,
      HealthResponse,
   },
};

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/.health", get(health_check))
      .route("/.sessions", get(sessions_debug))
}

/// Health check endpoint returning session pool statistics.
async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
   Json(state.api.get_session_health().await)
}

/// Detailed sessions debug endpoint.
async fn sessions_debug(State(state): State<AppState>) -> Json<DebugResponse> {
   Json(state.api.get_session_debug().await)
}
