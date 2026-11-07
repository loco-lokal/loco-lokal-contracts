//! Request preprocessing that Rails does before routing:
//!
//! - `Rack::MethodOverride`: a POST form with `_method=put|patch|delete` is routed as that
//!   method (`button_to ... method: :delete`, `form_for` updates).
//! - `protect_from_forgery with: :exception`: unsafe requests outside `/api` need a valid
//!   authenticity token, from the `X-CSRF-Token` header (Turbo, Vue) or the
//!   `authenticity_token` field of a URL-encoded form, and a matching `Origin` header when
//!   one is sent. Multipart forms must send the header; Turbo does, and later phases add
//!   field parsing where a page posts multipart without Turbo.
//!
//! URL-encoded bodies are buffered (up to [`MAX_FORM_BYTES`]) and handed on unchanged.

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{errors::AppError, session::Session, state::AppState};

pub const MAX_FORM_BYTES: usize = 10 * 1024 * 1024;

/// Paths without CSRF protection: `ActionController::API` controllers.
fn csrf_exempt(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

fn is_urlencoded(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';').next().is_some_and(|m| {
                m.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        })
}

/// Must run inside the session layer and outside the router.
pub async fn forms_layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();

    let mut form_token = None;
    let body = if parts.method == Method::POST && is_urlencoded(&parts.headers) {
        let Ok(bytes) = to_bytes(body, MAX_FORM_BYTES).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };

        for (key, value) in form_urlencoded::parse(&bytes) {
            match key.as_ref() {
                "_method" => {
                    if let Some(method) = override_method(&value) {
                        parts.method = method;
                    }
                }
                "authenticity_token" => form_token = Some(value.into_owned()),
                _ => {}
            }
        }
        Body::from(bytes)
    } else {
        body
    };

    if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS)
        && !csrf_exempt(parts.uri.path())
    {
        let Some(session) = parts.extensions.get::<Session>().cloned() else {
            return AppError::Internal(anyhow::anyhow!("session layer is not installed"))
                .into_response();
        };

        let header_token = parts
            .headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let token_ok = session.has_csrf_secret()
            && header_token
                .iter()
                .chain(form_token.iter())
                .any(|token| loco_rails_compat::csrf::verify(&session.csrf_secret(), token));

        if !token_ok || !origin_ok(&parts.headers, state.config.force_ssl) {
            tracing::warn!(path = parts.uri.path(), "invalid authenticity token");
            // ActionController::InvalidAuthenticityToken is rendered as 422.
            return AppError::Status(StatusCode::UNPROCESSABLE_ENTITY).into_response();
        }
    }

    next.run(Request::from_parts(parts, body)).await
}

fn override_method(value: &str) -> Option<Method> {
    match value.to_ascii_uppercase().as_str() {
        "PUT" => Some(Method::PUT),
        "PATCH" => Some(Method::PATCH),
        "DELETE" => Some(Method::DELETE),
        _ => None,
    }
}

/// `forgery_protection_origin_check`: an `Origin` header, when present, must be this
/// host. `null` (privacy-sensitive contexts) fails, as in Rails.
fn origin_ok(headers: &HeaderMap, force_ssl: bool) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };

    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let schemes: &[&str] = if force_ssl || forwarded_https {
        &["https"]
    } else {
        &["http", "https"]
    };

    schemes
        .iter()
        .any(|scheme| origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn checks_origin_against_host() {
        assert!(origin_ok(&headers(&[("host", "localhost:8080")]), false));
        assert!(origin_ok(
            &headers(&[
                ("host", "localhost:8080"),
                ("origin", "http://localhost:8080")
            ]),
            false
        ));
        assert!(!origin_ok(
            &headers(&[("host", "localhost:8080"), ("origin", "http://evil.test")]),
            false
        ));
        assert!(!origin_ok(
            &headers(&[("host", "sign.test"), ("origin", "http://sign.test")]),
            true
        ));
        assert!(!origin_ok(
            &headers(&[("host", "sign.test"), ("origin", "null")]),
            false
        ));
    }

    #[test]
    fn overrides_only_known_methods() {
        assert_eq!(override_method("delete"), Some(Method::DELETE));
        assert_eq!(override_method("PATCH"), Some(Method::PATCH));
        assert_eq!(override_method("get"), None);
    }
}
