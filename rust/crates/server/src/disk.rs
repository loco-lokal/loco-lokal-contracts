//! `ActiveStorage::Blob#url` for the disk service (`DiskService#generate_url`) and
//! `ActiveStorage::DiskController#show`, which serves those URLs.
//!
//! The URL carries `ActiveStorage.verifier.generate({key, disposition, content_type,
//! service_name}, purpose: :blob_key, expires_in: service_urls_expire_in)`. Blobs on
//! cloud services get the blob proxy URL instead of a presigned one.

use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use loco_storage::Blob;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};

use crate::{errors::AppError, state::AppState};

/// `config.active_storage.service_urls_expire_in`.
pub const URLS_EXPIRE_IN_MINUTES: i64 = 240;

/// `ActiveStorage.content_types_allowed_inline`.
const ALLOWED_INLINE: &[&str] = &[
    "image/webp",
    "image/avif",
    "image/png",
    "image/gif",
    "image/jpeg",
    "image/tiff",
    "image/bmp",
    "image/vnd.adobe.photoshop",
    "image/vnd.microsoft.icon",
    "application/pdf",
];

/// `ActiveStorage.content_types_to_serve_as_binary`.
const SERVE_AS_BINARY: &[&str] = &[
    "text/html",
    "image/svg+xml",
    "application/postscript",
    "application/x-shockwave-flash",
    "text/xml",
    "application/xml",
    "application/xhtml+xml",
    "application/mathml+xml",
    "text/cache-manifest",
    "application/javascript",
    "text/javascript",
    "application/ecmascript",
    "text/ecmascript",
    "application/wasm",
];

/// RFC 5987 `attr-char`s, as `ActionDispatch::Http::ContentDisposition` keeps them.
const RFC5987: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// Path segments as `url_for` escapes the `*filename` glob.
const PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@');

/// `ActionDispatch::Http::ContentDisposition.format`.
#[must_use]
pub fn content_disposition(disposition: &str, filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "{disposition}; filename=\"{ascii}\"; filename*=UTF-8''{}",
        utf8_percent_encode(filename, RFC5987)
    )
}

/// `Blob#content_type_for_serving` and `#forced_disposition_for_serving`.
fn serving(blob: &Blob) -> (String, &'static str) {
    let content_type = blob
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let binary = SERVE_AS_BINARY.contains(&content_type.as_str());
    let content_type = if binary {
        "application/octet-stream".to_owned()
    } else {
        content_type
    };
    let disposition = if binary || !ALLOWED_INLINE.contains(&content_type.as_str()) {
        "attachment"
    } else {
        "inline"
    };
    (content_type, disposition)
}

/// `blob.url` (a loaded blob, with its uuid ensured for the proxy fallback). `origin` is
/// the request's base URL (`ActiveStorage::Current.url_options`).
#[must_use]
pub fn blob_url(state: &AppState, origin: &str, blob: &Blob) -> String {
    if !state.storage.is_disk(&blob.service_name) {
        return crate::files::url_for(
            state,
            blob.uuid.as_deref().unwrap_or_default(),
            &blob.filename,
            None,
        );
    }
    let (content_type, disposition) = serving(blob);
    let token = state.secrets.active_storage_verifier.generate(
        &json!({
            "key": blob.key,
            "disposition": content_disposition(disposition, &blob.filename),
            "content_type": content_type,
            "service_name": blob.service_name,
        }),
        Some("blob_key"),
        Some(Utc::now() + Duration::minutes(URLS_EXPIRE_IN_MINUTES)),
    );
    format!(
        "{origin}/disk/{token}/{}",
        utf8_percent_encode(&blob.filename, PATH)
    )
}

/// `GET /disk/:encoded_key/*filename` (Rails sets `active_storage.routes_prefix = ''`).
pub async fn show(
    State(state): State<AppState>,
    Path((encoded_key, _filename)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let Ok(key) = state
        .secrets
        .active_storage_verifier
        .verify(&encoded_key, Some("blob_key"))
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let field = |name: &str| key.get(name).and_then(Value::as_str).unwrap_or_default();
    let service_name = match field("service_name") {
        "" => state.storage.default_service().to_owned(),
        name => name.to_owned(),
    };
    let data = match state
        .storage
        .download_key(&service_name, field("key"))
        .await
    {
        Ok(data) => data,
        Err(err) if err.is_not_found() => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(err) => return Err(err.into()),
    };
    let mut response = data.into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, field("content_type")),
        (header::CONTENT_DISPOSITION, field("disposition")),
        (header::CACHE_CONTROL, "public, max-age=31536000"),
    ] {
        if let Ok(value) = HeaderValue::from_str(value)
            && !value.is_empty()
        {
            headers.insert(name, value);
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_content_disposition_like_rails() {
        assert_eq!(
            content_disposition("inline", "sig näme.png"),
            "inline; filename=\"sig n?me.png\"; filename*=UTF-8''sig%20n%C3%A4me.png"
        );
    }
}
