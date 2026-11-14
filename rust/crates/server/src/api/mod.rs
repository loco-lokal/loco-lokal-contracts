//! The JSON API (`Api::*` controllers under `/api`).

pub(crate) mod create;
pub mod events;
pub mod params;
pub mod permit;
pub mod serialize;
pub mod submissions;
pub mod submitters;
pub mod templates;
pub mod time;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use loco_db::{templates::Page, users};
use serde_json::{Value, json};

use crate::{
    auth::{ApiAuth, Auth},
    errors::AppError,
    state::AppState,
};

pub use params::Params;

/// `ApiBaseController::DEFAULT_LIMIT` and `MAX_LIMIT`.
pub const DEFAULT_LIMIT: i64 = 10;
pub const MAX_LIMIT: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/user", get(user))
        .route("/api/templates", get(templates::index))
        .route(
            "/api/templates/{id}",
            get(templates::show)
                .put(templates::update)
                .patch(templates::update)
                .delete(templates::destroy),
        )
        .route("/api/templates/{id}/clone", post(templates::clone))
        .route(
            "/api/templates/{id}/submissions",
            get(submissions::index_for_template).post(submissions::create_for_template),
        )
        .route(
            "/api/submissions",
            get(submissions::index).post(submissions::create),
        )
        .route("/api/submissions/init", post(submissions::create_init))
        .route("/api/submissions/emails", post(submissions::create_emails))
        .route(
            "/api/submissions/{id}",
            get(submissions::show)
                .put(submissions::update)
                .patch(submissions::update)
                .delete(submissions::destroy),
        )
        .route(
            "/api/submissions/{id}/documents",
            get(submissions::documents),
        )
        .route("/api/submitters", get(submitters::index))
        .route(
            "/api/submitters/{id}",
            get(submitters::show)
                .put(submitters::update)
                .patch(submitters::update),
        )
        .route("/api/submitter_form_views", post(submitters::form_view))
        .route("/api/events/form/{type}", get(events::form))
        .route("/api/events/submission/{type}", get(events::submission))
}

/// `render json:` with Rails' escaping: `<`, `>`, `&`, U+2028 and U+2029 become `\uXXXX`
/// (`ActiveSupport.escape_html_entities_in_json`). They only occur inside strings, so
/// escaping the serialized text is safe.
#[must_use]
pub fn render(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, Body::from(rails_json(value))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// `value.to_json` as Active Support encodes it (see [`render`]).
#[must_use]
pub fn rails_json(value: &Value) -> String {
    escape_json_html(&value.to_string())
}

/// Active Support's escaping of serialized JSON (see [`render`]).
fn escape_json_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// `JSON.generate(value)`: like `Value::to_string`, with floats as Ruby writes them
/// (`12.0`, `1.0e-05`).
#[must_use]
pub fn ruby_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Number(n) if n.is_f64() => {
                out.push_str(&crate::signing::pages::ruby_float(
                    n.as_f64().unwrap_or_default(),
                ));
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    write(item, out);
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// [`render`] with floats as Ruby writes them ([`ruby_json`]).
#[must_use]
pub fn render_ruby_floats(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, Body::from(escape_json_html(&ruby_json(value)))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

#[must_use]
pub fn ok(value: &Value) -> Response {
    render(StatusCode::OK, value)
}

/// `render json: { error: message }, status:`.
#[must_use]
pub fn error(status: StatusCode, message: impl Into<String>) -> Response {
    render(status, &json!({ "error": message.into() }))
}

/// `422 {"error": message}`.
#[must_use]
pub fn unprocessable(message: impl Into<String>) -> Response {
    error(StatusCode::UNPROCESSABLE_ENTITY, message)
}

/// `ApiBaseController#paginate` with an id cursor.
#[must_use]
pub fn page(params: &Params) -> Page {
    Page {
        limit: params
            .get("limit")
            .and_then(params::string)
            .map_or(DEFAULT_LIMIT, |s| params::to_i(&s))
            .min(MAX_LIMIT),
        after: params.int("after"),
        before: params.int("before"),
    }
}

/// `{count:, next: last.id, prev: first.id}`.
#[must_use]
pub fn pagination(ids: &[i64]) -> Value {
    json!({
        "count": ids.len(),
        "next": ids.last(),
        "prev": ids.first(),
    })
}

/// A record the current user may not access: `403`, naming the other API key when the
/// record belongs to the linked testing or production account
/// (`ApiBaseController#access_denied_error_message`).
pub async fn access_denied(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    model: &str,
    id: i64,
    account_id: Option<i64>,
) -> Result<Response, AppError> {
    let not_authorized = || error(StatusCode::FORBIDDEN, "Not authorized");

    let token = headers
        .get("x-auth-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| !t.trim().is_empty());
    let Some(account_id) = account_id.filter(|_| token) else {
        return Ok(not_authorized());
    };
    if !users::is_linked_testing_counterpart(
        &state.db,
        auth.account.id,
        auth.account.testing,
        account_id,
    )
    .await?
    {
        return Ok(not_authorized());
    }

    let plural = format!("{}s", model.to_lowercase());
    let message = if auth.account.testing {
        format!(
            "{model} {id} not found using testing API key; Use production API key to access production {plural}."
        )
    } else {
        format!(
            "{model} {id} not found using production API key; Use testing API key to access testing {plural}."
        )
    };
    Ok(error(StatusCode::FORBIDDEN, message))
}

/// `Api::UsersController#show`.
pub async fn user(ApiAuth(auth): ApiAuth) -> Response {
    let user = auth.user;
    ok(&json!({
        "id": user.id,
        "first_name": user.first_name,
        "last_name": user.last_name,
        "email": user.email,
    }))
}

/// `ActiveRecord::Base.sanitize_sql_like(keyword.downcase)` wrapped in `%…%`.
#[must_use]
pub fn like_pattern(keyword: &str) -> String {
    let escaped = keyword
        .to_lowercase()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_rails_json() {
        let response = render(StatusCode::OK, &json!({"a": "<b>&\u{2028}"}));
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn builds_like_patterns() {
        assert_eq!(like_pattern("A_b%"), "%a\\_b\\%%");
    }
}
