//! Prefilled attachment fields of the API (`Submitters::NormalizeValues`
//! `normalize_attachment_value` and `find_or_build_attachment`, `DownloadUtils`, and
//! `NormalizeParamUtils.save_default_value_attachments!`).
//!
//! Values are base64 data or `https` URLs of PNG or JPEG images and PDF files. Other file
//! types that Marcel would recognize, and typed signatures (text rendered in a font,
//! `GenerateFontImage`), answer 422 "not supported yet".

use base64::Engine as _;
use loco_storage::{Attachment, Blob};
use serde_json::{Map, Value, json};

use super::values::Pending;
use crate::{errors::AppError, state::AppState};

/// `DownloadUtils::LOCALHOSTS`.
const LOCALHOSTS: &[&str] = &[
    "0.0.0.0",
    "127.0.0.1",
    "127.0.1.1",
    "localhost",
    "localhost.localdomain",
    "::1",
    "[::1]",
    "ip6-localhost",
    "ip6-loopback",
    "127.0.0.0",
    "127.255.255.255",
    "::",
    "0:0:0:0:0:0:0:1",
    "[0:0:0:0:0:0:0:1]",
    "0000:0000:0000:0000:0000:0000:0000:0001",
    "[0000:0000:0000:0000:0000:0000:0000:0001]",
    "::0",
    "0::0",
    "::ffff:127.0.0.1",
    "[::ffff:127.0.0.1]",
    "::ffff:7f00:1",
    "[::ffff:7f00:1]",
    "local",
    "localhost.local",
    "ip6-localnet",
    "ip6-allnodes",
    "ip6-allrouters",
];

/// Faraday's `follow_redirects` limit.
const REDIRECT_LIMIT: usize = 3;

/// An attachment the values refer to that is created with the submitter.
#[derive(Clone, Debug)]
pub struct NewAttachment {
    pub uuid: String,
    pub blob_id: i64,
}

/// An attachment the normalization found or built.
struct Found {
    uuid: String,
    blob_id: i64,
    new: bool,
}

/// A file type the Rust server can store: the content type and `Marcel::TYPE_EXTS` first
/// extension.
fn detect(data: &[u8]) -> Option<(&'static str, &'static str)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if data.starts_with(b"%PDF-") {
        Some(("application/pdf", "pdf"))
    } else {
        None
    }
}

/// Whether `Marcel::MimeType.for` would recognize the data as a type that [`detect`] does
/// not handle (common magic numbers of its database).
fn other_known_type(data: &[u8]) -> bool {
    const MAGIC: &[&[u8]] = &[
        b"GIF87a",
        b"GIF89a",
        b"BM",
        b"II*\0",
        b"MM\0*",
        b"PK\x03\x04",
        b"\xD0\xCF\x11\xE0",
        b"{\\rtf",
        b"<?xml",
        b"<svg",
        b"%!PS",
        b"ID3",
        b"OggS",
        b"fLaC",
        b"7z\xBC\xAF",
        b"Rar!",
        b"\x1F\x8B",
        b"MZ",
        b"\0\0\x01\0",
        b"8BPS",
        b"wOFF",
        b"wOF2",
        b"\0\x01\0\0",
        b"OTTO",
        b"\x1A\x45\xDF\xA3",
    ];
    let lower: Vec<u8> = data.iter().take(64).map(u8::to_ascii_lowercase).collect();
    MAGIC.iter().any(|m| data.starts_with(m))
        || (data.len() > 12 && (&data[0..4] == b"RIFF" || &data[4..8] == b"ftyp"))
        || lower.starts_with(b"<html")
        || lower.starts_with(b"<!doctype html")
}

/// `Base64.decode64`: characters outside the alphabet are skipped, decoding stops at the
/// padding, and trailing bits that don't make a byte are dropped.
fn decode64(value: &str) -> Vec<u8> {
    let mut clean: String = value
        .chars()
        .take_while(|c| *c != '=')
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    if clean.len() % 4 == 1 {
        clean.pop();
    }
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(clean.as_bytes())
        .unwrap_or_default()
}

/// `String#first(200)`.
fn first_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

/// `find_blob_by_checksum`: a blob with the checksum whose first attachment is a submitter
/// attachment of the account.
async fn find_blob_by_checksum(
    state: &AppState,
    checksum: &str,
    account_id: i64,
) -> Result<Option<Blob>, AppError> {
    let Some(blob) = Blob::find_by_checksum(&state.db, checksum).await? else {
        return Ok(None);
    };
    let Some(attachment) = Attachment::first_for_blob(&state.db, blob.id).await? else {
        return Ok(None);
    };
    if attachment.record_type != "Submitter" || attachment.name != "attachments" {
        return Ok(None);
    }
    let owner = loco_db::submissions::find_submitter(&state.db, attachment.record_id).await?;
    Ok(owner.filter(|s| s.account_id == account_id).map(|_| blob))
}

/// `ActiveStorage::Blob.create_and_upload!`, with the metadata the analyze job adds.
async fn create_blob(
    state: &AppState,
    data: Vec<u8>,
    filename: &str,
    content_type: &str,
) -> Result<Blob, AppError> {
    // `config.active_storage.analyzers = []`: the analyze job only marks the blob.
    let metadata = json!({ "identified": true, "analyzed": true });
    let mut blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            data.into(),
            filename,
            Some(content_type),
            metadata,
        )
        .await?;
    blob.ensure_uuid(&state.db).await?;
    Ok(blob)
}

/// `find_or_create_blob_from_base64` and `find_or_create_blob_from_url` after the data is
/// at hand.
async fn blob_for_data(
    state: &AppState,
    account_id: i64,
    data: Vec<u8>,
    filename: impl FnOnce(&str) -> String,
) -> Result<Result<Blob, String>, AppError> {
    let checksum = loco_storage::checksum(&data);
    if let Some(blob) = find_blob_by_checksum(state, &checksum, account_id).await? {
        return Ok(Ok(blob));
    }
    let Some((content_type, extension)) = detect(&data) else {
        return Ok(Err(
            "Prefilling attachments of this file type is not supported yet".into(),
        ));
    };
    let filename = filename(extension);
    Ok(Ok(create_blob(state, data, &filename, content_type).await?))
}

/// `DownloadUtils.validate_uri!`.
fn validate_url(url: &reqwest::Url, original: &str) -> Result<(), String> {
    if url.scheme() != "https" || url.port().is_some_and(|p| p != 443) {
        return Err(format!("Error loading: {original}. Only HTTPS is allowed."));
    }
    if url
        .host_str()
        .is_some_and(|host| LOCALHOSTS.contains(&host))
    {
        return Err(format!(
            "Error loading: {original}. Can't download from localhost."
        ));
    }
    Ok(())
}

/// `DownloadUtils.call(url, validate: true)`, following up to three redirects.
async fn download(state: &AppState, url: &str) -> Result<Result<Vec<u8>, String>, AppError> {
    let Ok(mut current) = reqwest::Url::parse(url) else {
        return Ok(Err(format!("Error loading: {url}")));
    };
    if let Err(message) = validate_url(&current, url) {
        return Ok(Err(message));
    }
    for _ in 0..=REDIRECT_LIMIT {
        let response = state.http.get(current.clone()).send().await?;
        let status = response.status();
        if status.is_redirection()
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            let next = current.join(location)?;
            if let Err(message) = validate_url(&next, next.as_str()) {
                return Ok(Err(message));
            }
            current = next;
            continue;
        }
        if status.as_u16() >= 400 {
            return Ok(Err(format!("Error loading: {url}")));
        }
        return Ok(Ok(response.bytes().await?.to_vec()));
    }
    Err(anyhow::anyhow!("too many redirects").into())
}

/// `find_or_build_attachment` with `purpose: :api`.
async fn find_or_build(
    state: &AppState,
    account_id: i64,
    for_submitter: Option<i64>,
    value: &Value,
    field_type: &str,
) -> Result<Result<Found, String>, AppError> {
    let Some(value) = value.as_str() else {
        return Err(anyhow::anyhow!("undefined method 'match?' for a non-string value").into());
    };
    let blob = if value.starts_with("http://") || value.starts_with("https://") {
        // `find_or_create_blob_from_url`
        let filename = reqwest::Url::parse(value)
            .ok()
            .and_then(|u| u.path().rsplit('/').next().map(str::to_owned))
            .unwrap_or_default();
        if let Some(extension) = crate::files::dangerous_extension(&filename) {
            return Ok(Err(format!("File type '.{extension}' is not allowed.")));
        }
        let data = match download(state, value).await? {
            Ok(data) => data,
            Err(message) => return Ok(Err(message)),
        };
        match blob_for_data(state, account_id, data, |_| filename.clone()).await? {
            Ok(blob) => blob,
            Err(message) => return Ok(Err(message)),
        }
    } else if matches!(field_type, "signature" | "initials") && value.chars().count() < 60 {
        return Ok(Err(format!(
            "Prefilling {field_type} fields with text is not supported yet"
        )));
    } else {
        let encoded = regex::Regex::new(r"\Adata:image/\w+;base64,")
            .expect("valid regex")
            .replace(value, "")
            .into_owned();
        let data = decode64(&encoded);
        if detect(&data).is_none() {
            if other_known_type(&data) {
                return Ok(Err(
                    "Prefilling attachments of this file type is not supported yet".into(),
                ));
            }
            // Marcel finds no type (`octet-stream`).
            if field_type == "image"
                && (value.starts_with("<html>") || value.starts_with("<!DOCTYPE"))
            {
                return Ok(Err(format!(
                    "HTML content is not allowed: {}...",
                    first_chars(value, 200)
                )));
            }
            return Ok(Err(format!(
                "Invalid value, url, base64 or text < 60 chars is expected: {}...",
                first_chars(value, 200)
            )));
        }
        let field_type = field_type.to_owned();
        match blob_for_data(state, account_id, data, |ext| format!("{field_type}.{ext}")).await? {
            Ok(blob) => blob,
            Err(message) => return Ok(Err(message)),
        }
    };

    if let Some(submitter_id) = for_submitter
        && let Some(existing) =
            Attachment::for_record(&state.db, "Submitter", submitter_id, "attachments")
                .await?
                .into_iter()
                .find(|a| a.blob_id == blob.id)
    {
        return Ok(Ok(Found {
            uuid: existing.uuid,
            blob_id: blob.id,
            new: false,
        }));
    }
    Ok(Ok(Found {
        uuid: uuid::Uuid::new_v4().to_string(),
        blob_id: blob.id,
        new: true,
    }))
}

/// `normalize_attachment_value` for each pending value, in order: an attachment of a blob
/// found earlier in the same values is reused. Sets the values to the attachment uuids and
/// returns the attachments to create.
pub async fn resolve(
    state: &AppState,
    account_id: i64,
    for_submitter: Option<i64>,
    pending: Vec<Pending>,
    values: &mut Map<String, Value>,
) -> Result<Result<Vec<NewAttachment>, String>, AppError> {
    let mut attachments: Vec<Found> = Vec::new();
    for item in pending {
        let resolved = if let Value::Array(items) = &item.value {
            let mut uuids = Vec::new();
            let mut built = Vec::new();
            for v in items {
                let found =
                    match find_or_build(state, account_id, for_submitter, v, &item.field_type)
                        .await?
                    {
                        Ok(found) => found,
                        Err(message) => return Ok(Err(message)),
                    };
                let found = match attachments.iter().find(|a| a.blob_id == found.blob_id) {
                    Some(a) => Found {
                        uuid: a.uuid.clone(),
                        blob_id: a.blob_id,
                        new: a.new,
                    },
                    None => found,
                };
                uuids.push(Value::String(found.uuid.clone()));
                built.push(found);
            }
            attachments.extend(built);
            Value::Array(uuids)
        } else {
            let found = match find_or_build(
                state,
                account_id,
                for_submitter,
                &item.value,
                &item.field_type,
            )
            .await?
            {
                Ok(found) => found,
                Err(message) => return Ok(Err(message)),
            };
            let found = match attachments.iter().find(|a| a.blob_id == found.blob_id) {
                Some(a) => Found {
                    uuid: a.uuid.clone(),
                    blob_id: a.blob_id,
                    new: a.new,
                },
                None => found,
            };
            let uuid = Value::String(found.uuid.clone());
            attachments.push(found);
            uuid
        };
        values.insert(item.field_uuid, resolved);
    }
    let mut new: Vec<NewAttachment> = Vec::new();
    for a in attachments.into_iter().filter(|a| a.new) {
        if !new.iter().any(|n| n.uuid == a.uuid) {
            new.push(NewAttachment {
                uuid: a.uuid,
                blob_id: a.blob_id,
            });
        }
    }
    Ok(Ok(new))
}

/// `save_default_value_attachments!`: the new attachments the submitter's values refer to
/// are attached to it (and touch it).
pub async fn save(
    db: &mut loco_db::PgConnection,
    attachments: &[NewAttachment],
    submitter: &loco_db::submissions::Submitter,
) -> Result<(), AppError> {
    let mut attached: Vec<&str> = Vec::new();
    if let Some(values) = submitter.values.as_object() {
        for value in values.values() {
            let items: Vec<&Value> = match value {
                Value::Array(items) => items.iter().collect(),
                other => vec![other],
            };
            for item in items {
                let Some(uuid) = item.as_str() else { continue };
                let Some(attachment) = attachments.iter().find(|a| a.uuid == uuid) else {
                    continue;
                };
                if attached.contains(&attachment.uuid.as_str()) {
                    continue;
                }
                Attachment::create_for_blob(
                    &mut *db,
                    "Submitter",
                    submitter.id,
                    "attachments",
                    attachment.blob_id,
                    &attachment.uuid,
                )
                .await?;
                attached.push(&attachment.uuid);
            }
        }
    }
    if !attached.is_empty() {
        loco_db::signing::touch_submitter(&mut *db, submitter.id).await?;
    }
    Ok(())
}
