//! HTTP server replacing the Rails application.

pub mod api;
pub mod auth;
pub mod config;
pub mod cookies;
pub mod disk;
pub mod email_variables;
pub mod errors;
pub mod files;
pub mod forms;
pub mod images;
pub mod l10n;
pub mod locale;
pub mod mail;
pub mod markdown;
pub mod page;
pub mod purge;
pub mod rate_limit;
pub mod request;
pub mod routes;
pub mod search;
pub mod session;
pub mod signing;
pub mod state;
pub mod validation;
pub mod webhooks;

use std::{convert::Infallible, path::Path};

use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use tower::ServiceExt as _;
use tower_http::{catch_panic::CatchPanicLayer, services::ServeDir, trace::TraceLayer};

pub use state::AppState;

/// Job context shared by all background jobs.
pub type JobContext = AppState;

/// Background jobs this server knows how to run.
#[must_use]
pub fn job_registry() -> loco_jobs::Registry<JobContext> {
    loco_jobs::Registry::default()
        .register::<webhooks::SendWebhook>()
        .register::<purge::PurgeBlobs>()
        .register::<search::ReindexSearchEntry>()
        .register::<mail::SendInvitation>()
        .register::<mail::SendDeclined>()
        .register::<signing::emails::SendCompleted>()
        .register::<signing::emails::SendDocumentsCopy>()
        .register::<signing::completion::ProcessSubmitterCompletion>()
}

/// The application. Like Rails' `ActionDispatch::Static`, files in `public/` are served
/// before sessions, method override and CSRF checks; everything else goes through them
/// to the routes.
pub fn router(state: AppState) -> Router {
    let pages = Router::new()
        .merge(routes::router())
        .route("/up", get(up))
        .fallback(not_found)
        .with_state(state.clone());

    // Method override must happen before routing, so these wrap the routed app.
    let pages = Router::new()
        .fallback_service(pages)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            forms::forms_layer,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            session::session_layer,
        ));

    let public_dir = state.config.app_root.join("public");
    let public = ServeDir::new(&public_dir);

    let app = tower::service_fn(move |request: Request| {
        let pages = pages.clone();
        let public = public.clone();
        let is_static = static_file(&public_dir, &request);
        async move {
            let response = if is_static {
                public
                    .oneshot(request)
                    .await
                    .map(IntoResponse::into_response)?
            } else {
                pages.oneshot(request).await?
            };
            Ok::<Response, Infallible>(response)
        }
    });

    Router::new()
        .fallback_service(app)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            errors::error_page_layer,
        ))
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn static_file(public_dir: &Path, request: &Request) -> bool {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return false;
    }
    let path = request.uri().path().trim_start_matches('/');
    !path.is_empty()
        && !path
            .split('/')
            .any(|segment| segment == ".." || segment.starts_with('.'))
        && public_dir.join(path).is_file()
}

async fn not_found() -> errors::AppError {
    errors::AppError::Status(StatusCode::NOT_FOUND)
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
