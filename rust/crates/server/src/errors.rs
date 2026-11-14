//! Error responses, matching `ErrorsController`: the static pages in `app/views/errors/`
//! for browsers, `{"status": …, "error": …}` JSON for API and JSON requests.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde_json::json;

use crate::state::AppState;

/// Handler-level error type. Unexpected errors are logged and returned as 500.
#[derive(Debug)]
pub enum AppError {
    Status(StatusCode),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self::Internal(err.into())
    }
}

impl AppError {
    /// For code outside requests (jobs) that reuses request helpers.
    #[must_use]
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Status(status) => anyhow::anyhow!("request failed with {status}"),
            Self::Internal(err) => err,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Status(status) => status,
            Self::Internal(err) => {
                tracing::error!(error = format!("{err:#}"), "request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        // Rendered with the request context by `render_error` (see `error_page_layer`).
        let mut response = status.into_response();
        response.extensions_mut().insert(ErrorPage);
        response
    }
}

/// Marks a bare status response so the error layer renders the error page for it.
#[derive(Clone, Copy)]
pub struct ErrorPage;

/// Middleware that replaces marked error responses with the HTML or JSON error body.
pub async fn error_page_layer(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let wants_json = wants_json(request.uri().path(), request.headers());
    let response = next.run(request).await;

    if response.extensions().get::<ErrorPage>().is_none() {
        return response;
    }

    render_error(&state, response.status(), wants_json)
}

fn wants_json(path: &str, headers: &HeaderMap) -> bool {
    path.starts_with("/api/")
        || std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|accept| accept.contains("application/json"))
}

fn render_error(state: &AppState, status: StatusCode, json: bool) -> Response {
    if json {
        let mut response = (status, Json(json!({ "status": status.as_u16() }))).into_response();
        let headers = response.headers_mut();
        for (name, value) in [
            ("access-control-allow-origin", "*"),
            (
                "access-control-allow-methods",
                "POST, GET, PUT, PATCH, DELETE, OPTIONS",
            ),
            ("access-control-allow-headers", "*"),
            ("access-control-max-age", "1728000"),
            ("access-control-allow-credentials", "true"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        return response;
    }

    let page = match status.as_u16() {
        404 => "404",
        422 => "422",
        500 => "500",
        // `ErrorsController` has no page for other statuses (such as 400 for
        // `ActionController::ParameterMissing`), so Rails answers with its failsafe.
        _ => return failsafe(),
    };
    let path = state
        .config
        .app_root
        .join("app/views/errors")
        .join(format!("{page}.html"));

    match std::fs::read_to_string(&path) {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => status.into_response(),
    }
}

/// `ActionDispatch::ShowExceptions#render_exception` when the error page itself fails.
fn failsafe() -> Response {
    let mut response = (
        StatusCode::INTERNAL_SERVER_ERROR,
        "500 Internal Server Error\nIf you are the administrator of this website, then please \
         read this web application's log file and/or the web server's log file to find out \
         what went wrong.",
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
