//! Signed file links: `ActiveStorage::Blob.proxy_url` and
//! `Api::ActiveStorageBlobsProxyController` (`/file/:signed_uuid/*filename`).

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use loco_db::{records, templates, users};
use loco_domain::ability::Action;
use loco_storage::{Attachment, Blob};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::{auth::MaybeApiAuth, errors::AppError, state::AppState};

/// `AccountConfig::DOWNLOAD_LINKS_EXPIRE_KEY`.
pub const DOWNLOAD_LINKS_EXPIRE_KEY: &str = "download_links_expire";
/// `AccountConfig::DOWNLOAD_LINKS_AUTH_KEY`.
pub const DOWNLOAD_LINKS_AUTH_KEY: &str = "download_links_auth";

/// Characters Rails' router keeps unescaped in a glob segment (`Journey::Router::Utils
/// .escape_path`): unreserved, sub-delims, `:`, `@` and `/`.
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
    .remove(b'@')
    .remove(b'/');

/// `Accounts::LINK_EXPIRES_AT` (`FILE_URLS_EXPIRE_MINUTES`, default 40).
fn link_ttl() -> Duration {
    let minutes = std::env::var("FILE_URLS_EXPIRE_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(40);
    Duration::minutes(minutes)
}

/// `Accounts.link_expires_at(account)`: links expire unless the account turned it off.
pub async fn link_expires_at(
    state: &AppState,
    account_id: i64,
) -> Result<Option<DateTime<Utc>>, AppError> {
    let value = users::account_config(&state.db, account_id, DOWNLOAD_LINKS_EXPIRE_KEY).await?;
    Ok(if value == Some(Value::Bool(false)) {
        None
    } else {
        Some(Utc::now() + link_ttl())
    })
}

/// `blob.signed_uuid(expires_at:)`: `[uuid, "blob", exp]` signed without purpose or
/// envelope expiry; the controller checks `exp` itself.
#[must_use]
pub fn signed_uuid(state: &AppState, blob_uuid: &str, expires_at: Option<DateTime<Utc>>) -> String {
    let mut data = vec![json!(blob_uuid), json!("blob")];
    if let Some(at) = expires_at {
        data.push(json!(at.timestamp()));
    }
    state
        .secrets
        .signed_id_verifier
        .generate(&Value::Array(data), None, None)
}

/// `ActiveStorage::Blob.proxy_url(blob, expires_at:)`.
pub async fn proxy_url(
    state: &AppState,
    blob: &mut Blob,
    expires_at: Option<DateTime<Utc>>,
) -> Result<String, AppError> {
    let uuid = blob.ensure_uuid(&state.db).await?;
    Ok(url_for(state, &uuid, &blob.filename, expires_at))
}

/// The proxy URL of a blob whose uuid is known.
#[must_use]
pub fn url_for(
    state: &AppState,
    blob_uuid: &str,
    filename: &str,
    expires_at: Option<DateTime<Utc>>,
) -> String {
    format!(
        "{}/file/{}/{}",
        state.default_url(),
        signed_uuid(state, blob_uuid, expires_at),
        utf8_percent_encode(filename, PATH)
    )
}

/// `FilenameUtils.dangerous_extension`.
#[must_use]
pub fn dangerous_extension(filename: &str) -> Option<String> {
    const DANGEROUS: &[&str] = &[
        "exe",
        "com",
        "bat",
        "cmd",
        "scr",
        "pif",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "msi",
        "msp",
        "hta",
        "cpl",
        "jar",
        "app",
        "deb",
        "rpm",
        "dmg",
        "pkg",
        "mpkg",
        "dll",
        "so",
        "dylib",
        "sys",
        "inf",
        "reg",
        "ps1",
        "psm1",
        "psd1",
        "ps1xml",
        "psc1",
        "pssc",
        "vb",
        "vba",
        "sh",
        "bash",
        "zsh",
        "fish",
        "run",
        "out",
        "bin",
        "elf",
        "gadget",
        "workflow",
        "lnk",
        "scf",
        "url",
        "desktop",
        "application",
        "action",
        "apk",
        "ipa",
        "xap",
        "appx",
        "appxbundle",
        "msix",
        "msixbundle",
        "diagcab",
        "diagpkg",
        "msc",
        "ocx",
        "drv",
        "ins",
        "isp",
        "mst",
        "paf",
        "prf",
        "shb",
        "shs",
        "slk",
        "ws",
        "wsc",
        "inf1",
        "inf2",
    ];
    let sanitized = sanitize_filename(filename);
    // `\.(ext)\W*\z`: the extension may be followed by trailing non-word characters.
    let trimmed = sanitized.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
    let (_, ext) = trimmed.rsplit_once('.')?;
    let ext = ext.to_lowercase();
    DANGEROUS.contains(&ext.as_str()).then_some(ext)
}

/// `ActiveStorage::Filename#sanitized`.
#[must_use]
pub fn sanitize_filename(filename: &str) -> String {
    filename
        .chars()
        .filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'))
        .map(|c| match c {
            '"' | '/' | '\\' | ':' | '*' | '?' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect()
}

struct BlobRequest<'a> {
    method: &'a Method,
    headers: &'a HeaderMap,
    fullpath: &'a str,
    disposition: Option<&'a str>,
}

#[derive(Deserialize)]
pub struct ShowParams {
    disposition: Option<String>,
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "Not authorized" })),
    )
        .into_response()
}

/// `GET /file/:signed_uuid/*filename` (and the legacy `/blobs_proxy/…`).
pub async fn show(
    State(state): State<AppState>,
    MaybeApiAuth(auth): MaybeApiAuth,
    Path((signed_uuid, _filename)): Path<(String, String)>,
    Query(params): Query<ShowParams>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Ok(Value::Array(data)) = state.secrets.signed_id_verifier.verify(&signed_uuid, None) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let (Some(blob_uuid), Some("blob")) = (
        data.first().and_then(Value::as_str),
        data.get(1).and_then(Value::as_str),
    ) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let exp = data.get(2).and_then(Value::as_i64);

    let Some(blob) = Blob::find_by_uuid(&state.db, blob_uuid).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if dangerous_extension(&blob.filename).is_some() {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }
    let Some(attachment) = Attachment::first_for_blob(&state.db, blob.id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };

    if !authorized(&state, auth.as_ref(), &attachment, exp).await? {
        return Ok(forbidden());
    }

    let fullpath = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |p| p.as_str());
    serve_blob(
        &state,
        &blob,
        &BlobRequest {
            method: &method,
            headers: &headers,
            fullpath,
            disposition: params.disposition.as_deref(),
        },
    )
    .await
}

/// `authorization_check!`.
async fn authorized(
    state: &AppState,
    auth: Option<&crate::auth::Auth>,
    attachment: &Attachment,
    exp: Option<i64>,
) -> Result<bool, AppError> {
    if attachment.name == "logo" {
        return Ok(true);
    }
    if exp.is_some_and(|exp| exp >= Utc::now().timestamp()) {
        return Ok(true);
    }

    // Preview images are attached to the document attachment, not to the record.
    let (record_type, record_id) = if attachment.record_type == "ActiveStorage::Attachment" {
        match Attachment::find(&state.db, attachment.record_id).await? {
            Some(parent) => (parent.record_type, parent.record_id),
            None => return Ok(false),
        }
    } else {
        (attachment.record_type.clone(), attachment.record_id)
    };
    let Some(owner) = records::owner(&state.db, &record_type, record_id).await? else {
        return Ok(false);
    };

    if let Some(auth) = auth {
        let ability = auth.ability();
        let can_read = if record_type == "Template" {
            let sharings = templates::sharings(&state.db, record_id).await?;
            ability.can_template(Action::Read, owner.account_id, &sharings)
        } else {
            owner
                .account_id
                .is_some_and(|id| ability.can_account_record(id))
        };
        if can_read {
            return Ok(true);
        }
    }

    if exp.is_none()
        && let Some(account_id) = owner.account_id
    {
        let require_auth = users::account_config(&state.db, account_id, DOWNLOAD_LINKS_AUTH_KEY)
            .await?
            .is_some_and(|v| v.as_bool().unwrap_or(false));
        let require_ttl = users::account_config(&state.db, account_id, DOWNLOAD_LINKS_EXPIRE_KEY)
            .await?
            != Some(Value::Bool(false));
        if !require_ttl && !require_auth {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Content types Active Storage never serves as their own type
/// (`content_types_to_serve_as_binary`, with the app's additions).
const SERVE_AS_BINARY: &[&str] = &[
    "text/html",
    "image/svg+xml",
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

/// `content_types_allowed_inline`.
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

fn content_type_for_serving(blob: &Blob) -> String {
    let content_type = blob
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    if SERVE_AS_BINARY.contains(&content_type) {
        "application/octet-stream".to_owned()
    } else {
        content_type.to_owned()
    }
}

/// `ActionDispatch::Http::ContentDisposition.format`.
fn content_disposition(disposition: &str, filename: &str) -> String {
    const ATTR: &AsciiSet = &NON_ALPHANUMERIC
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
    let escaped: String = filename
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .flat_map(|c| match c {
            '\\' | '"' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!(
        "{disposition}; filename=\"{escaped}\"; filename*=UTF-8''{}",
        utf8_percent_encode(filename, ATTR)
    )
}

/// `send_blob_stream` / `send_blob_byte_range_data`, inside `http_cache_forever`.
async fn serve_blob(
    state: &AppState,
    blob: &Blob,
    request: &BlobRequest<'_>,
) -> Result<Response, AppError> {
    let content_type = content_type_for_serving(blob);
    // `blob.forced_disposition_for_serving || params[:disposition] || "inline"`.
    let raw = blob.content_type.as_deref().unwrap_or_default();
    let forced = SERVE_AS_BINARY.contains(&raw) || !ALLOWED_INLINE.contains(&raw);
    let disposition = if forced {
        "attachment"
    } else {
        request.disposition.unwrap_or("inline")
    };
    let disposition = HeaderValue::from_str(&content_disposition(
        disposition,
        &sanitize_filename(&blob.filename),
    ))?;

    let range = request
        .headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|r| !r.trim().is_empty());

    let mut response = if let Some(range) = range {
        let data = state.storage.download(blob).await?;
        let mut response = match parse_range(range, data.len()) {
            Some((start, end)) => {
                let mut response = (
                    StatusCode::PARTIAL_CONTENT,
                    Body::from(data.slice(start..=end)),
                )
                    .into_response();
                let h = response.headers_mut();
                h.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes {start}-{end}/{}", data.len()))?,
                );
                h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type)?);
                h.insert(header::CONTENT_DISPOSITION, disposition);
                response
            }
            None => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
        };
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        response
    } else {
        // `http_cache_forever`: a weak ETag of the request path and a fixed Last-Modified.
        let digest = hex::encode(sha2::Sha256::digest(request.fullpath.as_bytes()));
        let etag = format!("W/\"{}\"", &digest[..32]);
        let not_modified = request
            .headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|tag| tag.trim() == etag || tag.trim() == "*")
            });

        let mut response = if not_modified {
            StatusCode::NOT_MODIFIED.into_response()
        } else if *request.method == Method::HEAD {
            let mut response = StatusCode::OK.into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(blob.byte_size));
            response
        } else {
            let data = state.storage.download(blob).await?;
            let mut response = Body::from(data).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, disposition);
            response
        };
        let h = response.headers_mut();
        h.insert(header::ETAG, HeaderValue::from_str(&etag)?);
        h.insert(
            header::LAST_MODIFIED,
            HeaderValue::from_static("Sat, 01 Jan 2011 00:00:00 GMT"),
        );
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if !not_modified {
            h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type)?);
        }
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=3155695200, public, immutable"),
        );
        response
    };

    let h = response.headers_mut();
    for (name, value) in [
        ("access-control-allow-origin", "*"),
        (
            "access-control-allow-methods",
            "POST, GET, PUT, PATCH, DELETE, OPTIONS",
        ),
        ("access-control-allow-headers", "*"),
        ("access-control-max-age", "1728000"),
        ("access-control-allow-credentials", "true"),
        ("x-robots-tag", "noindex"),
        ("x-content-type-options", "nosniff"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    Ok(response)
}

/// A single `bytes=start-end` range (`Rack::Utils.get_byte_ranges`); multiple ranges are
/// not used by the app's clients.
fn parse_range(range: &str, len: usize) -> Option<(usize, usize)> {
    let spec = range.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix: usize = suffix.parse().ok()?;
            (len.saturating_sub(suffix), len - 1)
        }
        (start, "") => (start.parse().ok()?, len - 1),
        (start, end) => (start.parse().ok()?, end.parse::<usize>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dangerous_extensions() {
        assert_eq!(dangerous_extension("setup.EXE").as_deref(), Some("exe"));
        assert_eq!(dangerous_extension("run.sh  ").as_deref(), Some("sh"));
        assert_eq!(dangerous_extension("contract.pdf"), None);
        assert_eq!(dangerous_extension("noext"), None);
    }

    #[test]
    fn parses_byte_ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("bytes=200-300", 100), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
    }

    #[test]
    fn formats_content_disposition_like_rails() {
        assert_eq!(
            content_disposition("attachment", "Vertrag Müller.pdf"),
            "attachment; filename=\"Vertrag M?ller.pdf\"; filename*=UTF-8''Vertrag%20M%C3%BCller.pdf"
        );
    }
}
