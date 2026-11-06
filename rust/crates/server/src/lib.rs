//! HTTP server replacing the Rails application.

pub mod config;

use axum::{Router, extract::State, http::StatusCode, routing::get};
use loco_db::PgPool;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/up", get(up))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Health check, equivalent to `rails/health#show`, plus a database round trip.
async fn up(State(state): State<AppState>) -> StatusCode {
    match loco_db::ping(&state.db).await {
        Ok(()) => StatusCode::OK,
        Err(err) => {
            tracing::error!(%err, "health check failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
