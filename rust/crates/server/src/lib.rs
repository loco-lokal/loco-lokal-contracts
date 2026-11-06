//! HTTP server replacing the Rails application.

pub mod config;
pub mod errors;
pub mod state;

use axum::{Router, extract::State, http::StatusCode, middleware, routing::get};
use tower_http::{catch_panic::CatchPanicLayer, services::ServeDir, trace::TraceLayer};

pub use state::AppState;

/// Job context shared by all background jobs.
pub type JobContext = AppState;

/// Background jobs this server knows how to run.
#[must_use]
pub fn job_registry() -> loco_jobs::Registry<JobContext> {
    loco_jobs::Registry::default()
}

pub fn router(state: AppState) -> Router {
    // Static files from `public/` (packs, icons, robots.txt), as Rails' public file server.
    let public = ServeDir::new(state.config.app_root.join("public")).fallback(tower::service_fn(
        |_| async {
            Ok::<_, std::convert::Infallible>(axum::response::IntoResponse::into_response(
                errors::AppError::Status(StatusCode::NOT_FOUND),
            ))
        },
    ));

    Router::new()
        .route("/up", get(up))
        .fallback_service(public)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            errors::error_page_layer,
        ))
        .layer(CatchPanicLayer::new())
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
