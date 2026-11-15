//! `Api::ToolsController`: `POST /api/tools/merge` (`PdfUtils.merge`) and
//! `POST /api/tools/verify` (`VerifyPdfSignature` with `Accounts.load_trusted_certs`).
//!
//! These answer 422 "not supported yet": merging documents with form fields, RFC 3161
//! timestamp signatures, signatures without RSA keys, changes after the last signature
//! that end with an `%%EOF` (`FPDF_HasOnlyDSSChanges`), and trusted certificates beyond
//! the generated ones (custom certificates, `CERTS`, `TRUSTED_CERTS`).

use axum::{extract::State, response::Response};
use base64::Engine as _;
use loco_db::signing;
use loco_pdf::{
    cms::{Pkcs7, Unsupported, certificate_info},
    signatures::{Merged, Signature},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{Params, create::attachments::decode64, ok, unprocessable};
use crate::{auth::ApiAuth, errors::AppError, state::AppState};

/// `Base64.encode64`: lines of 60 characters, each ending with a newline.
fn encode64(data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 60 + 1);
    for chunk in encoded.as_bytes().chunks(60) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out
}

fn unsupported(err: &Unsupported) -> Response {
    unprocessable(err.to_string())
}

/// `POST /api/tools/merge`.
pub async fn merge(
    State(_state): State<AppState>,
    ApiAuth(_auth): ApiAuth,
    params: Params,
) -> Result<Response, AppError> {
    let files = match params.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(files)) => files.clone(),
        Some(Value::String(s)) if s.trim().is_empty() => Vec::new(),
        Some(_) => return Err(anyhow::anyhow!("files is not an array").into()),
    };
    if files.is_empty() {
        return Ok(unprocessable("Files are required"));
    }
    if files.len() < 2 {
        return Ok(unprocessable("At least 2 files are required"));
    }
    let files: Vec<Vec<u8>> = files
        .iter()
        .map(|f| decode64(&crate::api::create::values::ruby_to_s(f)))
        .collect();
    let merged = tokio::task::spawn_blocking(move || {
        loco_pdf::with_shared(|pdfium| loco_pdf::signatures::merge(pdfium, &files))
    })
    .await???;
    match merged {
        Merged::Pdf(pdf) => Ok(ok(&json!({ "data": encode64(&pdf) }))),
        Merged::WithForm => Ok(unprocessable(
            "Merging documents with form fields is not supported yet",
        )),
    }
}

/// `Accounts.load_trusted_certs`: the public keys of the generated certificate and its CAs.
async fn trusted_keys(
    state: &AppState,
    account_id: i64,
) -> Result<Result<Vec<Vec<u8>>, Unsupported>, AppError> {
    if std::env::var("CERTS").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "{}")
        || std::env::var("TRUSTED_CERTS").is_ok_and(|v| !v.trim().is_empty())
    {
        return Ok(Err(Unsupported(
            "Verifying with CERTS or TRUSTED_CERTS certificates".into(),
        )));
    }
    let Some(encrypted) = signing::encrypted_config_with_fallback(
        &state.db,
        account_id,
        crate::state::ESIGN_CERTS_KEY,
    )
    .await?
    else {
        // `OpenSSL::X509::Certificate.new(nil)` raises.
        return Err(anyhow::anyhow!("no esign_certs").into());
    };
    let value: Value = serde_json::from_str(&state.secrets.encryptor.decrypt(&encrypted)?)?;
    if value
        .get("custom")
        .and_then(Value::as_array)
        .is_some_and(|c| {
            c.iter()
                .any(|e| loco_domain::submit::present(e.get("data")))
        })
    {
        return Ok(Err(Unsupported(
            "Verifying with custom signing certificates".into(),
        )));
    }
    let mut keys = Vec::new();
    for key in ["cert", "sub_ca", "root_ca"] {
        let pem = value.get(key).and_then(Value::as_str).unwrap_or_default();
        let der =
            loco_pdf::cms::pem_to_der(pem).ok_or_else(|| anyhow::anyhow!("{key}: invalid PEM"))?;
        let info =
            certificate_info(&der).ok_or_else(|| anyhow::anyhow!("{key}: invalid certificate"))?;
        keys.push(info.public_key);
    }
    Ok(Ok(keys))
}

/// `trusted_certificate?`: the signer's public key is one of the trusted ones.
fn trusted(pkcs7: &Pkcs7, keys: &[Vec<u8>]) -> bool {
    pkcs7
        .signer_certificate()
        .and_then(certificate_info)
        .is_some_and(|c| keys.contains(&c.public_key))
}

/// `covers_signed_revision?`: two ranges around exactly the hex `/Contents`.
fn covers_signed_revision(signature: &Signature, file: &[u8]) -> bool {
    let range = &signature.byte_range;
    if range.len() != 4 || range.iter().any(|v| *v < 0) || range[0] > 0 {
        return false;
    }
    let size = i64::try_from(file.len()).unwrap_or(i64::MAX);
    let contents_len = i64::try_from(signature.contents.len()).unwrap_or(i64::MAX);
    if signature.signed_end() > size || range[2] != range[1] + contents_len * 2 + 2 {
        return false;
    }
    let (Ok(start), Ok(end)) = (usize::try_from(range[1]), usize::try_from(range[2])) else {
        return false;
    };
    let Some(gap) = file.get(start..end) else {
        return false;
    };
    let expected = format!("<{}>", hex::encode(&signature.contents));
    gap.eq_ignore_ascii_case(expected.as_bytes())
}

/// `signed_data`: the bytes of the ranges.
fn signed_data(file: &[u8], range: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    for pair in range.chunks(2) {
        let (Ok(offset), Some(length)) = (usize::try_from(pair[0]), pair.get(1)) else {
            continue;
        };
        let available = file.len().saturating_sub(offset);
        let length = usize::try_from(*length).unwrap_or(0).min(available);
        if let Some(bytes) = file.get(offset..offset + length) {
            out.extend_from_slice(bytes);
        }
    }
    out
}

/// `verified_signature?`.
fn verified(signature: &Signature, file: &[u8]) -> Result<bool, Unsupported> {
    if !covers_signed_revision(signature, file) {
        return Ok(false);
    }
    let Some(pkcs7) = Pkcs7::parse(&signature.contents) else {
        return Ok(false);
    };
    if signature.sub_filter == "ETSI.RFC3161" {
        return Err(Unsupported("Verifying timestamp signatures".into()));
    }
    // `verify_contents`
    if pkcs7.digest_algorithms == 0 {
        return Ok(false);
    }
    pkcs7.verify(&signed_data(file, &signature.byte_range))
}

/// `parse_time`: the `/M` date as `Time.strptime(…, '%Y%m%d%H%M%S%z')` renders in JSON.
fn parse_time(value: &str) -> Result<Value, AppError> {
    if value.trim().is_empty() {
        return Ok(Value::Null);
    }
    let time = value.replace('\'', "");
    let time = time.strip_prefix("D:").unwrap_or(&time);
    let offset = time.get(14..).unwrap_or_default();
    let offset = if offset.starts_with(['+', '-']) {
        offset
    } else {
        "+0000"
    };
    let first: String = time.chars().take(14).collect();
    let parsed = chrono::DateTime::parse_from_str(&format!("{first}{offset}"), "%Y%m%d%H%M%S%z")
        .map_err(|e| anyhow::anyhow!("invalid signature time {value}: {e}"))?;
    Ok(json!(parsed.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()))
}

/// `SignerInfo#signed_time` as JSON (a UTC time).
fn signed_time(pkcs7: &Pkcs7) -> Option<Value> {
    let time = pkcs7.signers.first()?.signing_time.as_deref()?;
    let parsed = chrono::NaiveDateTime::parse_from_str(time, "%Y%m%d%H%M%S").ok()?;
    Some(json!(parsed.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()))
}

fn message(text: &str, status: Option<&str>) -> Value {
    json!({ "type": status.unwrap_or("info"), "content": text })
}

/// `POST /api/tools/verify`.
pub async fn verify(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    params: Params,
) -> Result<Response, AppError> {
    let Some(file) = params.get("file").filter(|v| !v.is_null()) else {
        // `Base64.decode64(nil)` raises.
        return Err(anyhow::anyhow!("file is missing").into());
    };
    let file = decode64(&crate::api::create::values::ruby_to_s(file));
    let keys = match trusted_keys(&state, auth.account.id).await? {
        Ok(keys) => keys,
        Err(err) => return Ok(unsupported(&err)),
    };
    let checksum = base64::engine::general_purpose::URL_SAFE.encode(Sha256::digest(&file));
    let found = signing::completed_document_exists(&state.db, &checksum).await?;

    let bytes = file.clone();
    let read = tokio::task::spawn_blocking(move || {
        loco_pdf::with_shared(|pdfium| loco_pdf::signatures::signatures(pdfium, &bytes))
    })
    .await??;
    let Ok(all) = read else {
        return Ok(unprocessable("Malformed PDF"));
    };
    let signatures: Vec<Signature> = all
        .into_iter()
        .filter(|s| s.byte_range.iter().any(|v| *v > 0) && !s.contents.is_empty())
        .collect();

    let mut results = Vec::new();
    if !signatures.is_empty() {
        let mut verified_flags = Vec::new();
        for signature in &signatures {
            match verified(signature, &file) {
                Ok(flag) => verified_flags.push(flag),
                Err(err) => return Ok(unsupported(&err)),
            }
        }
        let parsed: Vec<Option<Pkcs7>> = signatures
            .iter()
            .map(|s| Pkcs7::parse(&s.contents))
            .collect();
        let verified_ids: Vec<usize> = (0..signatures.len())
            .filter(|i| verified_flags[*i])
            .collect();
        let trusted_ids: Vec<usize> = verified_ids
            .iter()
            .copied()
            .filter(|i| parsed[*i].as_ref().is_some_and(|p| trusted(p, &keys)))
            .collect();
        let candidates = if trusted_ids.is_empty() {
            &verified_ids
        } else {
            &trusted_ids
        };
        // `max_by(&:signed_end)`: the first of the latest.
        let last = candidates
            .iter()
            .copied()
            .fold(None::<usize>, |best, i| match best {
                Some(b) if signatures[b].signed_end() >= signatures[i].signed_end() => Some(b),
                _ => Some(i),
            });
        // `unsigned_changes?`
        let mut unsigned_changes = false;
        if let Some(last) = last {
            let end = usize::try_from(signatures[last].signed_end()).unwrap_or(0);
            let rest = file.get(end..).unwrap_or_default();
            // `String#strip` also drops NUL bytes.
            let strip = |b: &u8| b.is_ascii_whitespace() || *b == 0 || *b == 0x0B;
            let start = rest.iter().position(|b| !strip(b)).unwrap_or(rest.len());
            let stop = rest
                .iter()
                .rposition(|b| !strip(b))
                .map_or(start, |p| p + 1);
            let rest = &rest[start..stop.max(start)];
            if !rest.is_empty() {
                if rest.ends_with(b"%%EOF") {
                    return Ok(unsupported(&Unsupported(
                        "Checking changes after the last signature".into(),
                    )));
                }
                unsigned_changes = true;
            }
        }

        let t = |key: &str| state.views.t(crate::locale::DEFAULT_LOCALE, key, &[]);
        for (i, signature) in signatures.iter().enumerate() {
            let Some(pkcs7) = &parsed[i] else {
                results.push(json!({
                    "verification_result": [message(&t("signature_verification_failed"), Some("error"))],
                    "signer_name": null,
                    "signing_reason": signature.reason,
                    "signing_time": parse_time(&signature.time)?,
                    "signature_type": signature.sub_filter,
                }));
                continue;
            };
            let mut messages = if verified_flags[i] {
                let certificate = if trusted(pkcs7, &keys) {
                    message(&t("signed_with_trusted_certificate"), Some("success"))
                } else {
                    message(&t("signed_with_external_certificate"), Some("error"))
                };
                vec![message(&t("signature_valid"), Some("success")), certificate]
            } else {
                vec![message(&t("signature_verification_failed"), Some("error"))]
            };
            if unsigned_changes && Some(i) == last {
                messages.push(message(
                    &t("contains_unsigned_changes_after_the_last_signature"),
                    Some("warning"),
                ));
            }
            let chain: Vec<String> = pkcs7
                .certificate_chain()
                .into_iter()
                .map(Option::unwrap_or_default)
                .collect();
            messages.push(message(
                &format!("Certificate chain: {}", chain.join(" -> ")),
                None,
            ));
            let signing_time = match signed_time(pkcs7) {
                Some(time) => time,
                None => parse_time(&signature.time)?,
            };
            results.push(json!({
                "verification_result": messages,
                "signer_name": pkcs7.signer_certificate().and_then(certificate_info).and_then(|c| c.common_name),
                "signing_reason": signature.reason,
                "signing_time": signing_time,
                "signature_type": signature.sub_filter,
            }));
        }
    }
    Ok(ok(&json!({
        "checksum_status": if found { "verified" } else { "not_found" },
        "signatures": results,
    })))
}
