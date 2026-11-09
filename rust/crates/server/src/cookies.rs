//! `cookies.encrypted[...]` values that Rails and the Rust server share (the start form's
//! `start_form_slug` and the email two-factor `email_2fa_slug`).

use axum::{
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};
use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::Value;

use crate::state::AppState;

/// `URI.encode_www_form_component` keeps `*-._` and alphanumerics.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'*')
    .remove(b'-')
    .remove(b'.')
    .remove(b'_');

fn raw(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| {
            percent_decode_str(&value.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        })
}

/// `cookies.encrypted[name]` as a string.
#[must_use]
pub fn encrypted(state: &AppState, headers: &HeaderMap, name: &str) -> Option<String> {
    let value = state
        .secrets
        .cookie_encryptor
        .decrypt(name, &raw(headers, name)?)?;
    match value {
        Value::String(s) => Some(s),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// `request.ssl?`: `FORCE_SSL` (`config.assume_ssl`) or `X-Forwarded-Proto: https`.
#[must_use]
pub fn ssl(state: &AppState, headers: &HeaderMap) -> bool {
    state.config.force_ssl
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
}

/// `cookies.encrypted[name] = { value:, expires:, httponly: true, secure: production }`.
/// Like Rails, a secure cookie is not written on a plain-http request.
pub fn set_encrypted(
    state: &AppState,
    headers: &HeaderMap,
    response: &mut Response,
    name: &str,
    value: &str,
    expires: DateTime<Utc>,
) {
    if state.config.production && !ssl(state, headers) {
        return;
    }
    let encrypted = state.secrets.cookie_encryptor.encrypt(
        name,
        &Value::String(value.to_owned()),
        Some(expires),
    );
    let mut cookie = format!(
        "{name}={}; path=/; expires={}; httponly",
        utf8_percent_encode(&encrypted, COMPONENT),
        expires.format("%a, %d %b %Y %H:%M:%S GMT"),
    );
    if state.config.production {
        cookie.push_str("; secure");
    }
    cookie.push_str("; samesite=lax");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}
